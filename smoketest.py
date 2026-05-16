import boto3, sys, time
from botocore.client import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://127.0.0.1:9123",
    aws_access_key_id="minioadmin",
    aws_secret_access_key="minioadmin",
    region_name="us-east-1",
    config=Config(s3={"addressing_style": "path"}, signature_version="s3v4"),
)

def show(label, fn):
    try:
        r = fn()
        print(f"OK  {label}: {r!r}")
    except Exception as e:
        print(f"ERR {label}: {e}")

show("create bucket", lambda: s3.create_bucket(Bucket="testbk"))
show("put object", lambda: s3.put_object(Bucket="testbk", Key="hello.txt", Body=b"Hello from boto3!", ContentType="text/plain"))
show("put large", lambda: s3.put_object(Bucket="testbk", Key="big.bin", Body=b"A" * 1024 * 250))
show("put nested", lambda: s3.put_object(Bucket="testbk", Key="folder/sub/file.txt", Body=b"nested data"))
show("head object", lambda: s3.head_object(Bucket="testbk", Key="hello.txt"))
show("get object", lambda: s3.get_object(Bucket="testbk", Key="hello.txt")["Body"].read())
show("list v2", lambda: [o["Key"] for o in s3.list_objects_v2(Bucket="testbk").get("Contents", [])])
show("list buckets", lambda: [b["Name"] for b in s3.list_buckets()["Buckets"]])
show("range get", lambda: s3.get_object(Bucket="testbk", Key="hello.txt", Range="bytes=6-10")["Body"].read())
show("delete object", lambda: s3.delete_object(Bucket="testbk", Key="hello.txt"))
show("delete batch", lambda: s3.delete_objects(Bucket="testbk", Delete={"Objects": [{"Key": "big.bin"}, {"Key": "folder/sub/file.txt"}]}))
show("delete bucket", lambda: s3.delete_bucket(Bucket="testbk"))
