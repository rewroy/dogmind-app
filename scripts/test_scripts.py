#!/usr/bin/env python3
"""What the packaging scripts promise about what they fetch and unpack: python -I scripts/test_scripts.py. Nothing is fetched, and all of it stays in temporary folders."""
import importlib.util
import os
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


class Dist(unittest.TestCase):
    def test_what_is_shipped_goes_to_dist_unless_the_environment_names_another_folder(self):
        with mock.patch.dict("os.environ"):
            os.environ.pop("DOGMIND_DIST", None)
            self.assertEqual(package.dist_folder(), package.ROOT / "dist")
            os.environ["DOGMIND_DIST"] = ""
            self.assertEqual(package.dist_folder(), package.ROOT / "dist")
        with tempfile.TemporaryDirectory() as folder, mock.patch.dict("os.environ", {"DOGMIND_DIST": folder}):
            self.assertEqual(package.dist_folder(), Path(folder).resolve())

    def test_no_path_in_it_is_dist_of_the_checkout_but_the_one_helper(self):
        # The folder, its symbols, the zip and the installer all come from `dist_folder()`: a path built from the checkout's `dist` beside it would write where the environment says not to.
        source = (HERE / "package.py").read_text(encoding="utf-8")
        self.assertEqual(source.count('ROOT / "dist"'), 1, "only dist_folder() names it")
        for name in ['"DogmindApp"', 'f"symbols-{version}"', 'f"DogmindApp-{version}.zip"', 'f"DogmindSetup-{version}.exe"']:
            self.assertIn(f"dist / {name}", source)


class Charts(unittest.TestCase):
    # What matplotlib's own matplotlibrc has of these keys: commented out, as its defaults, with CRLF line ends, one with a comment after it that goes on over the next lines.
    STOCK = (
        "## for font.serif, font.sans-serif, and font.monospace, the first element of\r\n"
        "#font.sans-serif: DejaVu Sans, Bitstream Vera Sans, Computer Modern Sans Serif, sans-serif\r\n"
        "#font.cursive:    Apple Chancery, cursive\r\n\r\n"
        "#axes.unicode_minus: True  # use Unicode for the minus symbol rather than hyphen.  See\r\n"
        "                           # https://en.wikipedia.org/wiki/Plus_and_minus_signs#Character_codes\r\n"
        "#axes.prop_cycle: cycler(color='tab10')\r\n"
    )

    def keys_as_matplotlib_reads_them(self, text):
        # (it reads this file for its defaults with the `#` off the start of each line, and a key said twice is a "Duplicate key" on stderr at every import)
        said = []
        for line in text.splitlines():
            line = (line[1:] if line.startswith("#") else line).split("#")[0].strip()
            if ":" in line:
                said.append(line.split(":", 1)[0].strip())
        return said

    def test_each_key_is_said_once_and_where_it_was(self):
        done = package.with_japanese_charts(self.STOCK)
        said = self.keys_as_matplotlib_reads_them(done)
        for key in ["font.sans-serif", "axes.unicode_minus"]:
            self.assertEqual(said.count(key), 1, key)
        self.assertIn("\r\nfont.sans-serif: BIZ UDGothic, DejaVu Sans\r\n", done)
        self.assertIn("\r\naxes.unicode_minus: False  # use Unicode for the minus symbol", done)
        # nothing else moved: the same lines but those two, with the line ends they had
        before, after = self.STOCK.split("\r\n"), done.split("\r\n")
        self.assertEqual(len(before), len(after))
        self.assertEqual([n for n, (a, b) in enumerate(zip(before, after)) if a != b], [1, 4])
        self.assertNotIn("\n", done.replace("\r\n", ""))

    def test_a_file_that_is_set_up_another_way_is_an_error_and_not_a_second_line(self):
        for text in ["", "#font.sans-serif: DejaVu Sans\r\n", "#axes.unicode_minus: True\r\n", self.STOCK + "#font.sans-serif: Arial\r\n", "font.sans-serif: Arial\r\n#axes.unicode_minus: True\r\n"]:
            with self.subTest(text), self.assertRaises(SystemExit):
                package.with_japanese_charts(text)


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
