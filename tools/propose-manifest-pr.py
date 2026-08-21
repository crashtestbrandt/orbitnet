#!/usr/bin/env python3
"""Open the binaries-manifest pull request for a release, and say so honestly when it cannot.

`main` is protected, so `release.yml` pushes the manifest to a branch and asks for a pull request. The
previous version piped `curl` into a printer, which exits 0 whatever the API answered -- so when GitHub
refused with "GitHub Actions is not permitted to create or approve pull requests", the step reported
success and v0.1.1's manifest sat on an unmerged branch nobody was told about.

Exit codes:
  0  the pull request was created, or one is already open for this branch
  1  anything else, with the API's own message and the remedy printed

Usage: propose-manifest-pr.py <repo> <head-branch> <base> <tag>
Reads the token from GH_TOKEN. The title and body are built here rather than passed in: they carry
backticks and a paragraph break, and a shell double-quoted string is the wrong place for either.
"""
import json, os, sys, urllib.error, urllib.request

if len(sys.argv) != 5:
    sys.exit("usage: propose-manifest-pr.py <repo> <head> <base> <tag>")
repo, head, base, tag = sys.argv[1:5]
title = "release: binaries manifest for %s" % tag
body = (
    "Digest of every asset published on `%s`, so a consumer can verify a download against the commit "
    "graph rather than against a checksum published beside the file it describes. Also stamps "
    "`addons/orbitnet/plugin.cfg`.\n\nOpened by `release.yml`; `main` is protected, so this cannot be "
    "a direct push." % tag
)
token = os.environ.get("GH_TOKEN") or sys.exit("propose-manifest-pr: GH_TOKEN is not set")


def api(path, payload=None):
    """Return (status, decoded-json). An HTTP error is a result here, not an exception."""
    req = urllib.request.Request("https://api.github.com/repos/%s/%s" % (repo, path))
    req.add_header("Authorization", "Bearer " + token)
    req.add_header("Accept", "application/vnd.github+json")
    if payload is not None:
        req.add_header("Content-Type", "application/json")
        req.data = json.dumps(payload).encode()
    try:
        r = urllib.request.urlopen(req)
        return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"message": raw.decode("utf-8", "replace")[:500]}


status, created = api("pulls", {"title": title, "head": head, "base": base, "body": body})
url = created.get("html_url") if isinstance(created, dict) else None
if url:
    print("opened %s" % url)
    sys.exit(0)

# A re-run of the same tag finds its own pull request already open. That is success, not a duplicate.
owner = repo.split("/")[0]
_, existing = api("pulls?state=open&head=%s:%s" % (owner, head))
if isinstance(existing, list) and existing:
    print("already open: %s" % existing[0].get("html_url"))
    sys.exit(0)

message = created.get("message", "") if isinstance(created, dict) else str(created)
errors = created.get("errors") if isinstance(created, dict) else None
print("::error::the manifest branch '%s' was pushed, but no pull request could be opened." % head)
print("::error::HTTP %s: %s" % (status, message))
if errors:
    print("::error::%s" % json.dumps(errors))
if "not permitted" in message:
    print("::error::Settings > Actions > General > 'Allow GitHub Actions to create and approve pull")
    print("::error::requests' is off for this repository or its organization. Turn it on, or open the")
    print("::error::pull request from '%s' by hand. The release itself published fine;" % head)
    print("::error::what is missing is the manifest commit that pins these assets to the commit graph.")
sys.exit(1)
