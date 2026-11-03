#!/usr/bin/env python3
"""What the packaging scripts promise about what they fetch and unpack: python -I scripts/test_scripts.py. Nothing is fetched, and all of it stays in temporary folders."""
import importlib.util
import tempfile
import unittest
import urllib.request
import zipfile
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


package = load("package")
fetch_tools = load("fetch_tools")


class Unpacking(unittest.TestCase):
    def setUp(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)
        self.target = self.root / "deep" / "er" / "target"
        self.target.mkdir(parents=True)

    def archive(self, *names):
        path = self.root / "a.zip"
        with zipfile.ZipFile(path, "w") as zip_file:
            for name in names:
                info = zipfile.ZipInfo(name)
                zip_file.writestr(info, "planted")
        return path

    def outside(self):
        return [path for path in self.root.rglob("*") if path.is_file() and path.name != "a.zip" and self.target not in path.parents]

    def test_a_name_that_leads_out_is_refused_in_each_of_its_spellings(self):
        for name in ["../x", "a/../../x", "..\\x", "a\\..\\..\\..\\x", "C:/x-planted", "C:\\x-planted", "\\x-planted", "/x-planted", "C:x-planted", "a:stream", ".. /x", "a/.../x"]:
            with self.subTest(name), self.assertRaises(SystemExit):
                package.extract(self.archive(name), self.target)
            self.assertEqual(self.outside(), [], name)

    def test_a_wheel_is_held_to_the_same(self):
        for name in ["../x", "a\\..\\..\\..\\x", "C:/x-planted", "\\x-planted", "pkg-1.data/purelib/a\\..\\..\\..\\x"]:
            with self.subTest(name), self.assertRaises(SystemExit):
                package.install_wheel(self.archive(name), self.target, [])
            self.assertEqual(self.outside(), [], name)

    def test_what_stays_inside_lands_where_it_says(self):
        package.extract(self.archive("a/b.txt", "c.txt"), self.target)
        self.assertEqual((self.target / "a" / "b.txt").read_text(), "planted")
        files = []
        package.install_wheel(self.archive("pkg/m.py", "pkg-1.data/purelib/n.py", "pkg-1.dist-info/LICENSE"), self.target, files)
        self.assertTrue((self.target / "pkg" / "m.py").is_file() and (self.target / "n.py").is_file())
        self.assertEqual(len(files), 3)

    def test_tools_only_takes_no_folder_with_other_things_in_it(self):
        (self.target / "thesis.docx").write_text("years")
        with self.assertRaises(SystemExit):
            package.refuse_unless_tools(self.target)
        (self.target / "python").mkdir()
        with self.assertRaises(SystemExit):
            package.refuse_unless_tools(self.target)
        (self.target / "thesis.docx").unlink()
        package.refuse_unless_tools(self.target)
        (self.target / ".open-to-sandbox").write_text("")
        package.refuse_unless_tools(self.target)


class Fetching(unittest.TestCase):
    def test_a_hash_that_is_not_the_one_pinned_leaves_nothing_in_the_cache(self):
        with tempfile.TemporaryDirectory() as folder:
            source = Path(folder) / "source"
            source.write_bytes(b"what came")
            cache = Path(folder) / "cache"
            with mock.patch.dict("os.environ", {"DOGMIND_TOOLS_CACHE": str(cache)}), mock.patch.object(urllib.request, "urlopen", lambda *_, **__: source.open("rb")):
                with self.assertRaises(SystemExit):
                    fetch_tools.fetch("https://example.invalid/x", "x.bin", "0" * 64)
                self.assertEqual(list(cache.iterdir()), [], "not the file, and not a part of it")
                wanted = fetch_tools.sha256(source)
                self.assertEqual(fetch_tools.fetch("https://example.invalid/x", "x.bin", wanted), cache / "x.bin")
                # What is in the cache and is not that is fetched again.
                (cache / "x.bin").write_bytes(b"changed since")
                self.assertEqual(fetch_tools.sha256(fetch_tools.fetch("https://example.invalid/x", "x.bin", wanted)), wanted)


if __name__ == "__main__":
    unittest.main()
