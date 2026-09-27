#!/usr/bin/env python3
"""Generate Herdr's principals map from Smarty's setup/org.json (smarty-dev#1515, stage c).

Each principal in org.json may carry

    "herdr": {
      "sshKeys": ["SHA256:<43 base64 chars>"],
      "tailscaleNodes": [{"node": "kates-mac.tailXXXX.ts.net", "login": "kate@example.com"}]
    }

The output is the map the Herdr server reads from /etc/herdr/principals.json. The server only
uses it when root owns it (and every directory above it) and group and others cannot write it,
so install it as root, for example:

    herdr_principals_from_org.py setup/org.json > principals.json
    sudo install -o root -g root -m 0644 -D principals.json /etc/herdr/principals.json

This script never installs anything. A person is labelled only when both a mapped key and a
mapped node (with its login) match, so never map a fleet host's key or node.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from typing import Any

FINGERPRINT = re.compile(r"^SHA256:[A-Za-z0-9+/]{43}$")
MAX_NAME = 80


class OrgError(ValueError):
    pass


def principals_from_org(org: dict[str, Any]) -> dict[str, Any]:
    principals = org.get("principals")
    if not isinstance(principals, list):
        raise OrgError("org.json has no principals list")
    out = []
    names: set[str] = set()
    key_owner: dict[str, str] = {}
    node_owner: dict[tuple[str, str], str] = {}
    for index, principal in enumerate(principals):
        if not isinstance(principal, dict):
            raise OrgError(f"principals[{index}] is not an object")
        herdr = principal.get("herdr")
        if herdr is None:
            continue
        name = principal.get("name")
        if not isinstance(name, str) or not name.strip() or name != name.strip():
            raise OrgError(f"principals[{index}] has no usable name")
        # The label is "**<name> (in Herdr):** "; a name the label shape cannot hold is refused.
        if len(name) > MAX_NAME or "\n" in name or "*" in name:
            raise OrgError(f"{name!r}: the name cannot appear in a label")
        if name in names:
            raise OrgError(f"{name!r} appears twice")
        names.add(name)
        if not isinstance(herdr, dict) or set(herdr) - {"sshKeys", "tailscaleNodes"}:
            raise OrgError(f"{name}: herdr must be {{sshKeys, tailscaleNodes}}")
        keys = herdr.get("sshKeys", [])
        nodes = herdr.get("tailscaleNodes", [])
        if not isinstance(keys, list) or not isinstance(nodes, list):
            raise OrgError(f"{name}: sshKeys and tailscaleNodes must be lists")
        for key in keys:
            if not isinstance(key, str) or not FINGERPRINT.match(key):
                raise OrgError(f"{name}: {key!r} is not an OpenSSH SHA256 fingerprint")
            if key_owner.setdefault(key, name) != name:
                raise OrgError(f"{key} is mapped to both {key_owner[key]} and {name}")
        clean_nodes = []
        for node in nodes:
            if (
                not isinstance(node, dict)
                or set(node) != {"node", "login"}
                or not all(isinstance(node[field], str) and node[field] for field in ("node", "login"))
            ):
                raise OrgError(f"{name}: each tailscale node needs exactly node and login")
            node_name = node["node"].rstrip(".")
            pair = (node_name, node["login"])
            if node_owner.setdefault(pair, name) != name:
                raise OrgError(f"{pair} is mapped to both {node_owner[pair]} and {name}")
            clean_nodes.append({"node": node_name, "login": node["login"]})
        if not keys or not clean_nodes:
            raise OrgError(f"{name}: needs at least one ssh key and one tailscale node")
        out.append({"name": name, "sshKeys": sorted(set(keys)), "tailscaleNodes": clean_nodes})
    return {"version": 1, "principals": out}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("org_json", help="path to setup/org.json")
    args = parser.parse_args(argv)
    try:
        with open(args.org_json, encoding="utf-8") as handle:
            result = principals_from_org(json.load(handle))
    except (OSError, json.JSONDecodeError, OrgError) as err:
        print(f"herdr_principals_from_org: {err}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout, indent=2, ensure_ascii=False)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
