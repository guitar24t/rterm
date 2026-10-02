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


def osc52(text, targets="c", end="\x07"):
    import base64
    return f"\x1b]52;{targets};{base64.b64encode(text.encode()).decode()}{end}".encode()


class Osc52(unittest.TestCase):
    def test_finds_copies_with_either_terminator(self):
        sc = rc.Osc52Scanner()
        data = b"before" + osc52("one") + b"mid\x1b[1m" + osc52("two \u00e9", end="\x1b\\") + b"after"
        self.assertEqual(sc.feed(data), [("one", "clipboard"), ("two \u00e9", "clipboard")])
        self.assertEqual(sc.carry, b"")

    def test_split_at_every_byte(self):
        stream = b"x" + osc52("split copy, " * 20) + b"y" + osc52("second", "p", "\x1b\\") + b"\x1b"
        sc = rc.Osc52Scanner()
        found = []
        for i in range(len(stream)):
            found += sc.feed(stream[i:i + 1])
        self.assertEqual(found, [("split copy, " * 20, "clipboard"), ("second", "primary")])

    def test_ignores_queries_cancelled_and_other_sequences(self):
        sc = rc.Osc52Scanner()
        data = (b"\x1b]52;c;?\x07"  # read request
                b"\x1b]52;c;aGVsbG8\x1b[0m"  # cancelled by another escape
                b"\x1b]2;title\x07\x1b]8;;http://x\x1b\\"  # other OSCs
                b"\x1b]52;c;\x07"  # empty
                + osc52("kept"))
        self.assertEqual(sc.feed(data), [("kept", "clipboard")])

    def test_long_copy_in_chunks_is_not_rescanned(self):
        text = "x" * 3_000_000
        stream = osc52(text, end="\x1b\\")
        sc = rc.Osc52Scanner()
        found = []
        import time
        started = time.monotonic()
        for i in range(0, len(stream), 4095):
            found += sc.feed(stream[i:i + 4095])
        self.assertEqual(found, [(text, "clipboard")])
        self.assertLess(time.monotonic() - started, 5)

    def test_holds_only_a_possible_prefix(self):
        sc = rc.Osc52Scanner()
        self.assertEqual(sc.feed(b"plain text \x1b]5"), [])
        self.assertEqual(sc.carry, b"\x1b]5")
        self.assertEqual(sc.feed(b"2;c;b2s=\x07"), [("ok", "clipboard")])
        self.assertEqual(sc.feed(b"no escapes here"), [])
        self.assertEqual(sc.carry, b"")


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
