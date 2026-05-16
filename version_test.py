import boto3, sys, hashlib
from botocore.client import Config
from botocore.exceptions import ClientError

s3 = boto3.client(
    "s3",
    endpoint_url="http://127.0.0.1:9123",
    aws_access_key_id="alice",
    aws_secret_access_key="alicepass",
    region_name="us-east-1",
    config=Config(s3={"addressing_style": "path"}, signature_version="s3v4"),
)

passed, failed = 0, 0
def t(label, fn):
    global passed, failed
    try:
        r = fn()
        print(f"OK   {label}: {r!r}"[:220])
        passed += 1
    except Exception as e:
        print(f"FAIL {label}: {type(e).__name__}: {e}")
        failed += 1

t("create bucket", lambda: s3.create_bucket(Bucket="vbk"))

# Versioning disabled by default
t("versioning before enable", lambda: s3.get_bucket_versioning(Bucket="vbk").get("Status"))

# Enable versioning
t("enable versioning", lambda: s3.put_bucket_versioning(
    Bucket="vbk", VersioningConfiguration={"Status": "Enabled"}))
t("versioning after enable", lambda: s3.get_bucket_versioning(Bucket="vbk")["Status"])

# Put same key three times, capture version ids
vids = []
def put_and_track(body):
    r = s3.put_object(Bucket="vbk", Key="story.txt", Body=body)
    vids.append(r["VersionId"])
    return r["VersionId"]

t("put v1", lambda: put_and_track(b"chapter one"))
t("put v2", lambda: put_and_track(b"chapter two"))
t("put v3", lambda: put_and_track(b"chapter three"))

t("latest GET returns v3", lambda: s3.get_object(Bucket="vbk", Key="story.txt")["Body"].read())
t("GET versionId=v1", lambda: s3.get_object(Bucket="vbk", Key="story.txt", VersionId=vids[0])["Body"].read())
t("GET versionId=v2", lambda: s3.get_object(Bucket="vbk", Key="story.txt", VersionId=vids[1])["Body"].read())

# List versions: should show 3, latest first, all is_latest=False except first
def list_v():
    r = s3.list_object_versions(Bucket="vbk")
    return [(v["VersionId"], v["IsLatest"], v.get("Size")) for v in r.get("Versions", [])]
t("list_object_versions count", lambda: len(s3.list_object_versions(Bucket="vbk")["Versions"]))
t("list_object_versions first is latest", lambda: s3.list_object_versions(Bucket="vbk")["Versions"][0]["IsLatest"])

# Delete without versionId -> creates a delete marker
def delete_marker():
    r = s3.delete_object(Bucket="vbk", Key="story.txt")
    return (r.get("DeleteMarker"), r.get("VersionId"))
t("delete creates marker", delete_marker)

# After delete marker, plain GET should 404
def get_after_marker():
    try:
        s3.get_object(Bucket="vbk", Key="story.txt")
        return "unexpected success"
    except ClientError as e:
        return e.response["Error"]["Code"]
t("GET after delete is NoSuchKey", get_after_marker)

# But specific old versions still readable
t("GET versionId=v2 after marker", lambda: s3.get_object(Bucket="vbk", Key="story.txt", VersionId=vids[1])["Body"].read())

# Delete markers appear in list_object_versions
def list_markers():
    r = s3.list_object_versions(Bucket="vbk")
    return len(r.get("DeleteMarkers", []))
t("DeleteMarkers in list", list_markers)

# Permanent delete of the marker should restore visibility
def find_marker_vid():
    r = s3.list_object_versions(Bucket="vbk")
    return r["DeleteMarkers"][0]["VersionId"]
marker_vid = find_marker_vid()
t("permanent delete marker", lambda: s3.delete_object(Bucket="vbk", Key="story.txt", VersionId=marker_vid))
t("GET works again", lambda: s3.get_object(Bucket="vbk", Key="story.txt")["Body"].read())

# Permanent delete of v1
t("permanent delete v1", lambda: s3.delete_object(Bucket="vbk", Key="story.txt", VersionId=vids[0]))
def v1_gone():
    try:
        s3.get_object(Bucket="vbk", Key="story.txt", VersionId=vids[0])
        return "unexpected"
    except ClientError as e:
        return e.response["Error"]["Code"]
t("v1 is gone", v1_gone)

# Cleanup all remaining versions then delete bucket
def cleanup():
    r = s3.list_object_versions(Bucket="vbk")
    objs = []
    for v in r.get("Versions", []):
        objs.append({"Key": v["Key"], "VersionId": v["VersionId"]})
    for v in r.get("DeleteMarkers", []):
        objs.append({"Key": v["Key"], "VersionId": v["VersionId"]})
    if objs:
        s3.delete_objects(Bucket="vbk", Delete={"Objects": objs})
    return "ok"
t("cleanup versions", cleanup)
t("delete bucket", lambda: s3.delete_bucket(Bucket="vbk"))

print(f"\n{passed} passed, {failed} failed")
sys.exit(0 if failed == 0 else 1)
