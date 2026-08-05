#!/usr/bin/env python3
"""SigV4 S3 ListObjectsV2 driver, stdlib only.

Proves the listing door does what a tool around the store actually needs: page a
pot with more keys than fit in one response, visit every key exactly once, group
by delimiter for a folder view, and mirror a prefix by listing it and reading
each object back at the size the listing promised.

The awkward-key cases are the point of the exercise as much as the paging is. A
key may hold `&`, `=`, a space, a literal `%`, or non-ASCII, and each of those
breaks a different naive implementation: a raw continuation token tears in half
on `&`, and a response that ignores `encoding-type` hands back a `%` that the
caller's own decoder then mangles.

Env: AK, SK (credentials), HOST (default 127.0.0.1:9000), N (objects to write).
Exits non-zero on the first failed expectation.
"""
import datetime, hashlib, hmac, os, sys, urllib.parse, urllib.request, urllib.error

# Stdlib ElementTree on purpose: these harnesses take no third-party dependency
# so they run on a bare box, and the only XML parsed here comes from a barmed
# this script's wrapper started itself, on loopback. Point it at a server you
# don't control and the usual untrusted-XML caveats apply — use defusedxml then.
import xml.etree.ElementTree as ET

HOST = os.environ.get("HOST", "127.0.0.1:9000")
ACCESS = os.environ["AK"]
SECRET = os.environ["SK"]
REGION, SERVICE = "us-east-1", "s3"
S3NS = "{http://s3.amazonaws.com/doc/2006-03-01/}"


def sha(b): return hashlib.sha256(b).hexdigest()
def hm(k, m): return hmac.new(k, m.encode(), hashlib.sha256).digest()


def signing_key(ds):
    k = hm(("AWS4" + SECRET).encode(), ds)
    return hm(hm(hm(k, REGION), SERVICE), "aws4_request")


def uri_encode(s, encode_slash=True):
    out = []
    for b in s.encode():
        ch = chr(b)
        if ch.isascii() and (ch.isalnum() or ch in "-_.~"):
            out.append(ch)
        elif ch == "/" and not encode_slash:
            out.append(ch)
        else:
            out.append("%%%02X" % b)
    return "".join(out)


def request(method, path, params=(), body=b""):
    """`path` and `params` are given raw; both forms are encoded here so the URL
    on the wire and the string that gets signed agree."""
    encoded = [(uri_encode(k), uri_encode(v)) for k, v in params]
    query = "&".join(f"{k}={v}" for k, v in encoded)
    canonical = "&".join(f"{k}={v}" for k, v in sorted(encoded))

    now = datetime.datetime.now(datetime.timezone.utc)
    amz, ds = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    ph = sha(body)
    signed = "host;x-amz-content-sha256;x-amz-date"
    ch = f"host:{HOST}\nx-amz-content-sha256:{ph}\nx-amz-date:{amz}\n"
    encoded_path = uri_encode(path, encode_slash=False)
    cr = f"{method}\n{encoded_path}\n{canonical}\n{ch}\n{signed}\n{ph}"
    scope = f"{ds}/{REGION}/{SERVICE}/aws4_request"
    sts = f"AWS4-HMAC-SHA256\n{amz}\n{scope}\n{sha(cr.encode())}"
    sig = hmac.new(signing_key(ds), sts.encode(), hashlib.sha256).hexdigest()
    auth = f"AWS4-HMAC-SHA256 Credential={ACCESS}/{scope}, SignedHeaders={signed}, Signature={sig}"

    url = f"http://{HOST}{encoded_path}" + (f"?{query}" if query else "")
    req = urllib.request.Request(
        url, data=body if method in ("PUT", "POST") else None, method=method
    )
    req.add_header("x-amz-date", amz)
    req.add_header("x-amz-content-sha256", ph)
    req.add_header("Authorization", auth)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


fails = []


def check(label, got, want):
    if got == want:
        print(f"PASS {label}")
        return True
    def brief(v):
        s = repr(v)
        return s if len(s) <= 140 else s[:140] + f"... ({len(v)} items)"
    print(f"FAIL {label}\n       got  {brief(got)}\n       want {brief(want)}")
    fails.append(label)
    return False


class Page:
    """One parsed ListBucketResult. Keys and prefixes arrive percent-encoded
    because every list here asks for `encoding-type=url`, so they are decoded
    on the way in — exactly what an SDK does for its caller."""

    def __init__(self, xml):
        root = ET.fromstring(xml)

        def text(tag, default=None):
            el = root.find(S3NS + tag)
            return default if el is None else el.text

        def dec(s):
            return urllib.parse.unquote(s or "")

        self.name = text("Name")
        self.key_count = int(text("KeyCount", "0"))
        self.max_keys = int(text("MaxKeys", "0"))
        self.truncated = text("IsTruncated") == "true"
        self.next_token = text("NextContinuationToken")
        self.contents = [
            {
                "key": dec(c.findtext(S3NS + "Key")),
                "size": int(c.findtext(S3NS + "Size")),
                "etag": (c.findtext(S3NS + "ETag") or "").strip('"'),
                "modified": c.findtext(S3NS + "LastModified"),
            }
            for c in root.findall(S3NS + "Contents")
        ]
        self.prefixes = [
            dec(p.findtext(S3NS + "Prefix")) for p in root.findall(S3NS + "CommonPrefixes")
        ]

    @property
    def keys(self):
        return [c["key"] for c in self.contents]


def put(bucket, key, body):
    st, resp = request("PUT", f"/{bucket}/{key}", (), body)
    if st != 200:
        raise SystemExit(f"PUT /{bucket}/{key} failed {st}: {resp[:200]!r}")


def listing(bucket, **opts):
    """One page. `token` and any of prefix/delimiter/max_keys/start_after."""
    params = [("list-type", "2"), ("encoding-type", "url")]
    for name, key in (
        ("prefix", "prefix"),
        ("delimiter", "delimiter"),
        ("start_after", "start-after"),
        ("token", "continuation-token"),
    ):
        if opts.get(name) is not None:
            params.append((key, str(opts[name])))
    if opts.get("max_keys") is not None:
        params.append(("max-keys", str(opts["max_keys"])))
    st, body = request("GET", f"/{bucket}", params)
    if st != 200:
        raise SystemExit(f"list /{bucket} failed {st}: {body[:300]!r}")
    return Page(body)


def walk(bucket, **opts):
    """Every page, following the server's tokens the way a mirror would.
    Guards against a cursor that never advances as well as one that skips."""
    keys, prefixes, pages = [], [], 0
    token = None
    while True:
        page = listing(bucket, token=token, **opts)
        keys.extend(page.keys)
        prefixes.extend(page.prefixes)
        pages += 1
        if pages > 500:
            raise SystemExit("paging did not terminate")
        # These two must agree, or a client either stops early or loops.
        if page.truncated != (page.next_token is not None):
            raise SystemExit(
                f"IsTruncated={page.truncated} but NextContinuationToken="
                f"{page.next_token!r}"
            )
        if not page.truncated:
            return keys, prefixes, pages
        token = page.next_token


def main():
    n = int(os.environ.get("N", "1150"))
    pot = "listing"

    expected = []
    for i in range(n):
        key = f"data/{i // 100:02d}/obj-{i:05d}.bin"
        put(pot, key, f"payload {i}".encode())
        expected.append(key)
    for key in ["notes/readme.txt", "notes/todo.txt", "top-level.txt"]:
        put(pot, key, b"x")
        expected.append(key)
    expected.sort()
    print(f"wrote {len(expected)} objects to /{pot}")

    # --- the whole pot, paged by the server's own tokens ---
    keys, _, pages = walk(pot)
    check("every key exactly once, in order", keys, expected)
    check("paged rather than answered in one go", pages, (len(expected) + 999) // 1000)

    first = listing(pot, max_keys=1)
    check("a page is bounded by max-keys", len(first.contents), 1)
    check("KeyCount matches what came back", first.key_count, 1)
    check("size is reported", first.contents[0]["size"], len(b"payload 0"))
    check("etag is the content id", first.contents[0]["etag"].startswith("blake3:"), True)
    # The field an SDK's strict parser rejects, and that looks fine read by eye.
    stamp = first.contents[0]["modified"]
    check("LastModified is UTC with milliseconds", len(stamp) == 24 and stamp.endswith("Z"), True)
    datetime.datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%S.%fZ")

    # --- max-keys above the ceiling is clamped, not refused ---
    check("max-keys is clamped to 1000", listing(pot, max_keys=99999).max_keys, 1000)

    # --- folder-style browsing ---
    top = listing(pot, delimiter="/")
    check("top level groups into folders", top.prefixes, ["data/", "notes/"])
    check("top level keeps its own keys", top.keys, ["top-level.txt"])
    check("a group counts toward KeyCount", top.key_count, 3)

    _, groups, _ = walk(pot, prefix="data/", delimiter="/")
    check("grouping under a prefix", groups, [f"data/{i:02d}/" for i in range(n // 100 + 1)])

    # One key per page, the boundary where a naive cursor repeats a group.
    _, groups, _ = walk(pot, delimiter="/", max_keys=1)
    check("a group is never repeated across pages", groups, ["data/", "notes/"])

    keys, _, _ = walk(pot, prefix="notes/")
    check("prefix filters", keys, ["notes/readme.txt", "notes/todo.txt"])

    # --- the mirror step: list a prefix, then read every object back ---
    copied = 0
    for page_keys in [walk(pot, prefix="data/00/")[0]]:
        for key in page_keys:
            st, body = request("GET", f"/{pot}/{key}")
            if st != 200:
                raise SystemExit(f"mirror GET {key} failed {st}")
            copied += 1
    listed = {c["key"]: c["size"] for c in listing(pot, prefix="data/00/").contents}
    sizes_ok = all(
        request("GET", f"/{pot}/{k}")[1].__len__() == size for k, size in list(listed.items())[:10]
    )
    check("a prefix mirrors, sizes matching the listing", (copied, sizes_ok), (100, True))

    # --- keys that break naive implementations ---
    odd = [
        "a&b=c.txt",            # a raw continuation token tears in half here
        "holiday+2026/photo.jpg",  # `+` is a literal plus, not a space
        "sp ace.txt",
        "100%-done.txt",        # mangled if encoding-type is ignored
        "quote'and\"quote",
        "unicode-ï.txt",
    ]
    for key in odd:
        put("oddkeys", key, b"x")
    keys, _, _ = walk("oddkeys")
    check("awkward keys round-trip", keys, sorted(odd))

    # One key per page forces a token to be minted from each of them in turn.
    keys, _, pages = walk("oddkeys", max_keys=1)
    check("a token survives every one of them", keys, sorted(odd))
    check("one page per key", pages, len(odd))

    keys, _, _ = walk("oddkeys", prefix="holiday+2026/")
    check("a plus in a prefix stays a plus", keys, ["holiday+2026/photo.jpg"])

    # --- empty, unknown, and malformed ---
    request("PUT", "/emptypot")
    check("an empty pot lists as empty", listing("emptypot").key_count, 0)
    check("an unknown pot is 404", request("GET", "/nosuchpot", [("list-type", "2")])[0], 404)
    check(
        "a bare GET on the pot path is 501, not a misleading empty page",
        request("GET", f"/{pot}")[0],
        501,
    )
    for bad in [("max-keys", "lots"), ("encoding-type", "rot13"), ("continuation-token", "zzz")]:
        st = request("GET", f"/{pot}", [("list-type", "2"), bad])[0]
        check(f"{bad[0]}={bad[1]} is a 400", st, 400)

    print()
    if fails:
        print(f"{len(fails)} FAILED: {fails}")
        return 1
    print("all listing checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
