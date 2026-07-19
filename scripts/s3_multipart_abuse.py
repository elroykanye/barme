#!/usr/bin/env python3
"""SigV4 S3 multipart abuse driver, stdlib only.

Hammers the multipart error paths over the wire — the ones that used to leak
chunk pins — with a fresh random part each cycle (so nothing dedups and any leak
shows up as disk growth). Also runs a few clean multipart uploads to confirm the
happy path works against a real signed S3 client, not just the in-process router.

Env: AK, SK (credentials), HOST (default 127.0.0.1:9000), N (abuse cycles).
Exits non-zero if a clean upload doesn't round-trip.
"""
import datetime, hashlib, hmac, os, sys, urllib.request, urllib.error

HOST = os.environ.get("HOST", "127.0.0.1:9000")
ACCESS = os.environ["AK"]
SECRET = os.environ["SK"]
REGION, SERVICE = "us-east-1", "s3"


def sha(b): return hashlib.sha256(b).hexdigest()
def hm(k, m): return hmac.new(k, m.encode(), hashlib.sha256).digest()


def signing_key(ds):
    k = hm(("AWS4" + SECRET).encode(), ds)
    return hm(hm(hm(k, REGION), SERVICE), "aws4_request")


def uri_encode(s):
    out = []
    for ch in s:
        o = ord(ch)
        if (0x41 <= o <= 0x5A) or (0x61 <= o <= 0x7A) or (0x30 <= o <= 0x39) or ch in "-_.~":
            out.append(ch)
        else:
            out.append("%%%02X" % o)
    return "".join(out)


def canonical_query(query):
    if not query:
        return ""
    pairs = []
    for kv in query.split("&"):
        if not kv:
            continue
        k, v = (kv.split("=", 1) + [""])[:2]
        pairs.append((uri_encode(k), uri_encode(v)))
    pairs.sort()
    return "&".join(f"{k}={v}" for k, v in pairs)


def request(method, path, query, body):
    now = datetime.datetime.now(datetime.timezone.utc)
    amz, ds = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    ph = sha(body)
    signed = "host;x-amz-content-sha256;x-amz-date"
    ch = f"host:{HOST}\nx-amz-content-sha256:{ph}\nx-amz-date:{amz}\n"
    cr = f"{method}\n{path}\n{canonical_query(query)}\n{ch}\n{signed}\n{ph}"
    scope = f"{ds}/{REGION}/{SERVICE}/aws4_request"
    sts = f"AWS4-HMAC-SHA256\n{amz}\n{scope}\n{sha(cr.encode())}"
    sig = hmac.new(signing_key(ds), sts.encode(), hashlib.sha256).hexdigest()
    auth = f"AWS4-HMAC-SHA256 Credential={ACCESS}/{scope}, SignedHeaders={signed}, Signature={sig}"
    url = f"http://{HOST}{path}" + (f"?{query}" if query else "")
    req = urllib.request.Request(url, data=body if method in ("PUT", "POST") else None, method=method)
    req.add_header("x-amz-date", amz)
    req.add_header("x-amz-content-sha256", ph)
    req.add_header("Authorization", auth)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def initiate(bucket, key):
    st, body = request("POST", f"/{bucket}/{key}", "uploads", b"")
    if st != 200:
        raise SystemExit(f"initiate failed {st}: {body[:200]!r}")
    t = body.decode()
    return t[t.find("<UploadId>") + 10 : t.find("</UploadId>")]


def upload_part(bucket, key, uid, pn, data):
    return request("PUT", f"/{bucket}/{key}", f"partNumber={pn}&uploadId={uid}", data)[0]


def complete(bucket, key, uid, parts):
    xml = "<CompleteMultipartUpload>" + "".join(
        f"<Part><PartNumber>{p}</PartNumber></Part>" for p in parts
    ) + "</CompleteMultipartUpload>"
    return request("POST", f"/{bucket}/{key}", f"uploadId={uid}", xml.encode())


def abort(bucket, key, uid):
    return request("DELETE", f"/{bucket}/{key}", f"uploadId={uid}", b"")[0]


def get(bucket, key):
    return request("GET", f"/{bucket}/{key}", "", b"")


def main():
    n = int(os.environ.get("N", "60"))

    # 1) Clean multipart upload, verify byte-exact round trip (real SDK-style flow).
    p1, p2 = os.urandom(400_000), os.urandom(150_000)
    uid = initiate("mp", "good.bin")
    assert upload_part("mp", "good.bin", uid, 1, p1) == 200
    assert upload_part("mp", "good.bin", uid, 2, p2) == 200
    st, _ = complete("mp", "good.bin", uid, [1, 2])
    assert st == 200, f"complete failed: {st}"
    st, got = get("mp", "good.bin")
    if st != 200 or got != p1 + p2:
        raise SystemExit(f"clean multipart did not round-trip: status={st} lenok={len(got)==len(p1)+len(p2)}")
    print("clean multipart round-trips over SigV4")

    # 2) Abuse: each cycle a fresh random part (no dedup), then either a
    #    complete naming a non-existent part (the F2 leak path) or an abort.
    bad = 0
    for i in range(n):
        key = f"abuse-{i}"
        uid = initiate("mp", key)
        if upload_part("mp", key, uid, 1, os.urandom(300_000)) != 200:
            bad += 1
            continue
        if i % 2 == 0:
            st, _ = complete("mp", key, uid, [999])  # InvalidPart -> 400
            if st != 400:
                bad += 1
        else:
            if abort("mp", key, uid) != 204:
                bad += 1
    print(f"ran {n} abusive multipart cycles ({bad} unexpected statuses)")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
