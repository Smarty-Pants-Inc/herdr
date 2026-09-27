import unittest

from scripts.herdr_principals_from_org import OrgError, principals_from_org

KATE_KEY = "SHA256:" + "k" * 43
PAUL_KEY = "SHA256:" + "p" * 43


def org(*principals):
    return {"org": "x", "principals": list(principals)}


def kate(**herdr):
    return {
        "name": "Kate",
        "role": "principal",
        "herdr": {
            "sshKeys": [KATE_KEY],
            "tailscaleNodes": [{"node": "kates-mac.tail0.ts.net.", "login": "kate@example.com"}],
            **herdr,
        },
    }


class PrincipalsFromOrgTest(unittest.TestCase):
    def test_maps_principals_with_herdr_identity_and_skips_others(self):
        result = principals_from_org(org({"name": "Paul", "role": "product owner"}, kate()))
        self.assertEqual(
            result,
            {
                "version": 1,
                "principals": [
                    {
                        "name": "Kate",
                        "sshKeys": [KATE_KEY],
                        "tailscaleNodes": [
                            {"node": "kates-mac.tail0.ts.net", "login": "kate@example.com"}
                        ],
                    }
                ],
            },
        )

    def test_refuses_a_key_or_node_shared_by_two_people(self):
        paul = kate()
        paul["name"] = "Paul"
        with self.assertRaises(OrgError):
            principals_from_org(org(kate(), paul))

    def test_requires_both_a_key_and_a_node(self):
        with self.assertRaises(OrgError):
            principals_from_org(org(kate(tailscaleNodes=[])))
        with self.assertRaises(OrgError):
            principals_from_org(org(kate(sshKeys=[])))

    def test_refuses_malformed_entries(self):
        with self.assertRaises(OrgError):
            principals_from_org(org(kate(sshKeys=["ssh-ed25519 AAAA"])))
        with self.assertRaises(OrgError):
            principals_from_org(org(kate(tailscaleNodes=[{"node": "n"}])))
        bad_name = kate()
        bad_name["name"] = "**Paul (in Herdr):**"
        with self.assertRaises(OrgError):
            principals_from_org(org(bad_name))


if __name__ == "__main__":
    unittest.main()
