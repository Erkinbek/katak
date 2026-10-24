"""Writes the small checked-in test fixtures in testdata/fixtures/.

The fixtures contain CRLF, CR, mixed line endings, BOMs, UTF-16, legacy
code pages and NUL bytes on purpose; .gitattributes marks them -text so Git
never rewrites them. Run from the repository root: python testdata/make_fixtures.py
"""

import os

OUT = os.path.join(os.path.dirname(__file__), "fixtures")

FIXTURES = {
    "crlf.csv": b'id,name,note\r\n1,Ann,"two\r\nlines"\r\n2,"Bob, Jr.","say ""hi"""\r\n',
    "cr_only.csv": b"id,name\r1,Ann\r2,Bob\r",
    "mixed_endings.csv": b"id,name\n1,Ann\r\n2,Bob\r3,Cid\n",
    "no_trailing_newline.csv": b"a,b\n1,2\n3,4",
    "bom_utf8.csv": b"\xef\xbb\xbfid;city\n1;Tashkent\n2;S\xc3\xa3o Paulo\n",
    "utf16le_bom.csv": b"\xff\xfe" + "id,имя\r\n1,Анна\r\n2,\"Борис, мл.\"\r\n".encode("utf-16-le"),
    "utf16be_bom.csv": b"\xfe\xff" + "id,name\n1,Zoë\n".encode("utf-16-be"),
    "cp1251.csv": "id;город\r\n1;Ташкент\r\n2;Москва\r\n".encode("cp1251"),
    "nul_bytes.csv": b"a,b\n1,x\x00y\n2,\xff\xfe\n",
    "sep_line.csv": b"sep=;\r\na;b\r\n1;2\r\n",
    "ragged.csv": b"a,b,c\n1,2,3\n4,5\n6,7,8,9\n\n10,11,12\n",
    "quotes.csv": b'a,b\n"x""y",z\n"",""\n"multi\nline",ok\n',
    "empty.csv": b"",
    "header_only.csv": b"id,name,value\n",
}


def main():
    os.makedirs(OUT, exist_ok=True)
    for name, data in FIXTURES.items():
        with open(os.path.join(OUT, name), "wb") as f:
            f.write(data)
    print(f"wrote {len(FIXTURES)} fixtures to {OUT}")


if __name__ == "__main__":
    main()
