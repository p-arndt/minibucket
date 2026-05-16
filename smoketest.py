import boto3, io, hashlib, os, sys
from boto3.s3.transfer import TransferConfig
from botocore.client import Config

ENDPOINT = "http://127.0.0.1:9123"

s3 = boto3.client(
    "s3",
    endpoint_url=ENDPOINT,
    aws_access_key_id="alice",
    aws_secret_access_key="alicepass",
    region_name="us-east-1",
    config=Config(s3={"addressing_style": "path"}, signature_version="s3v4"),
)

passed = 0
failed = 0

def t(label, fn):
    global passed, failed
    try:
        r = fn()
        print(f"OK   {label}: {r!r}"[:200])
        passed += 1
    except Exception as e:
        print(f"FAIL {label}: {type(e).__name__}: {e}")
        failed += 1

t("create bucket",             lambda: s3.create_bucket(Bucket="testbk")["Location"])
t("put plain",                 lambda: s3.put_object(Bucket="testbk", Key="hello.txt", Body=b"Hello!", ContentType="text/plain")["ETag"])
t("put nested",                lambda: s3.put_object(Bucket="testbk", Key="dir/a/b.txt", Body=b"nested")["ETag"])

# 12 MB body => boto3 default TransferConfig uses multipart for >8MB.
big = os.urandom(12 * 1024 * 1024)
big_md5 = hashlib.md5(big).hexdigest()

def upload_multipart():
    buf = io.BytesIO(big)
    cfg = TransferConfig(multipart_threshold=5 * 1024 * 1024, multipart_chunksize=5 * 1024 * 1024)
    s3.upload_fileobj(buf, "testbk", "big.bin", Config=cfg)
    return "uploaded"

t("multipart upload (12MB)",   upload_multipart)
t("multipart head",            lambda: s3.head_object(Bucket="testbk", Key="big.bin")["ContentLength"])
t("multipart get matches",     lambda: hashlib.md5(s3.get_object(Bucket="testbk", Key="big.bin")["Body"].read()).hexdigest() == big_md5)

t("copy object",               lambda: s3.copy_object(Bucket="testbk", Key="hello-copy.txt", CopySource={"Bucket": "testbk", "Key": "hello.txt"})["CopyObjectResult"]["ETag"])
t("get copied",                lambda: s3.get_object(Bucket="testbk", Key="hello-copy.txt")["Body"].read())

t("put tagging",               lambda: s3.put_object_tagging(Bucket="testbk", Key="hello.txt", Tagging={"TagSet":[{"Key":"env","Value":"dev"},{"Key":"owner","Value":"alice"}]}))
t("get tagging",               lambda: {t["Key"]:t["Value"] for t in s3.get_object_tagging(Bucket="testbk", Key="hello.txt")["TagSet"]})
t("delete tagging",            lambda: s3.delete_object_tagging(Bucket="testbk", Key="hello.txt"))

t("list v2",                   lambda: sorted(o["Key"] for o in s3.list_objects_v2(Bucket="testbk").get("Contents", [])))
t("range get",                 lambda: s3.get_object(Bucket="testbk", Key="hello.txt", Range="bytes=2-4")["Body"].read())

t("list buckets",              lambda: [b["Name"] for b in s3.list_buckets()["Buckets"]])

# Cleanup
t("delete all",                lambda: s3.delete_objects(Bucket="testbk", Delete={"Objects":[{"Key":k} for k in
                                  ["hello.txt","hello-copy.txt","dir/a/b.txt","big.bin"]]})["Deleted"])
t("delete bucket",             lambda: s3.delete_bucket(Bucket="testbk"))

print(f"\n{passed} passed, {failed} failed")
sys.exit(0 if failed == 0 else 1)
