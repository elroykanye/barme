#!/usr/bin/env python3
"""Drive barme through boto3-generated presigned PUT and GET URLs.

Env: AK, SK, and optional ENDPOINT (default http://127.0.0.1:9000).
Requires boto3. Exits non-zero on the first failed expectation.
"""
import os
import urllib.error
import urllib.parse
import urllib.request

import boto3
from botocore.config import Config


ENDPOINT = os.environ.get("ENDPOINT", "http://127.0.0.1:9000")
ACCESS = os.environ["AK"]
SECRET = os.environ["SK"]
BUCKET = "presign-proof"
KEY = "sdk-upload.txt"
BODY = b"boto3 presigned PUT and GET reached barme"


client = boto3.client(
    "s3",
    endpoint_url=ENDPOINT,
    aws_access_key_id=ACCESS,
    aws_secret_access_key=SECRET,
    region_name="us-east-1",
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)

put_url = client.generate_presigned_url(
    "put_object",
    Params={"Bucket": BUCKET, "Key": KEY, "ContentType": "text/plain"},
    ExpiresIn=300,
)
put = urllib.request.Request(
    put_url,
    method="PUT",
    data=BODY,
    headers={"Content-Type": "text/plain"},
)
with urllib.request.urlopen(put) as response:
    if response.status != 200:
        raise AssertionError(f"presigned PUT returned {response.status}")

get_url = client.generate_presigned_url(
    "get_object",
    Params={"Bucket": BUCKET, "Key": KEY},
    ExpiresIn=300,
)
with urllib.request.urlopen(get_url) as response:
    received = response.read()
    if response.status != 200 or received != BODY:
        raise AssertionError(
            f"presigned GET returned {response.status} and {received!r}"
        )

parsed = urllib.parse.urlsplit(get_url)
tampered = urllib.parse.urlunsplit(
    parsed._replace(path=parsed.path.replace(KEY, "tampered.txt"))
)
try:
    urllib.request.urlopen(tampered)
except urllib.error.HTTPError as error:
    if error.code != 403:
        raise AssertionError(f"tampered URL returned {error.code}, expected 403")
else:
    raise AssertionError("tampered URL was accepted")

print("PASS: boto3 presigned PUT and GET round-trip; tampered path rejected")
