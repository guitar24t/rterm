"""Tests for rterm-connect.py: python3 -m unittest discover -s contrib"""

import importlib.util
import os
import unittest

_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "rterm-connect.py")
_spec = importlib.util.spec_from_file_location("rterm_connect", _path)
rc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rc)


class ParseListing(unittest.TestCase):
    def test_json(self):
        text = (
            '[{"name":"main","status":"attached","created":1,"age":5,"age_text":"5s",'
            '"cols":80,"rows":24,"pid":9,"command":"-zsh","title":"vim  a b"},'
            '{"name":"bare","status":"detached","created":1,"age":5,"age_text":"5s",'
            '"cols":80,"rows":24,"pid":9,"command":"-zsh","title":""},'
            '{"name":"old","status":"unknown","error":"started by a different rterm version"}]'
        )
        self.assertEqual(rc.parse_listing(text), [
            rc.Session("main", "attached", "5s", "80x24", "vim  a b"),
            rc.Session("bare", "detached", "5s", "80x24", "-zsh"),
            rc.Session("old", "unknown", "", "", "started by a different rterm version"),
        ])

    def test_table_from_rterm_0_1(self):
        text = (
            "NAME     STATUS     AGE  SIZE    TITLE\n"
            "main     attached  3h05m  120x40  vim  README.md\n"
            "work     detached     9s  80x24   -zsh\n"
            "broken   ?                        Connection refused (os error 61)\n"
        )
        self.assertEqual(rc.parse_listing(text), [
            rc.Session("main", "attached", "3h05m", "120x40", "vim  README.md"),
            rc.Session("work", "detached", "9s", "80x24", "-zsh"),
            rc.Session("broken", "unknown", "", "", "Connection refused (os error 61)"),
        ])

    def test_empty(self):
        self.assertEqual(rc.parse_listing("no sessions\n"), [])
        self.assertEqual(rc.parse_listing("[]"), [])


class Names(unittest.TestCase):
    def test_suggest_name(self):
        s = lambda *names: [rc.Session(n, "detached") for n in names]
        self.assertEqual(rc.suggest_name([]), "main")
        self.assertEqual(rc.suggest_name(s("main")), "1")
        self.assertEqual(rc.suggest_name(s("main", "1", "2")), "3")

    def test_valid_names_match_rterm(self):
        for ok in ("main", "a", "work-1", "x_y.z", "me@host", "a+b"):
            self.assertTrue(rc.NAME_RE.match(ok), ok)
        for bad in ("", ".hidden", "-x", "a/b", "a b", "ü", "x" * 65, "main\n"):
            self.assertFalse(rc.NAME_RE.match(bad), bad)


if __name__ == "__main__":
    unittest.main()
