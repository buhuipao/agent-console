"""Check the public site's links, assets, and search metadata with the stdlib."""

import json
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlsplit
from xml.etree import ElementTree


class Page(HTMLParser):
    def __init__(self, source):
        super().__init__()
        self.tags = []
        self.schema = ""
        self.in_schema = False
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        self.tags.append((tag, attrs))
        if tag == "script":
            assert attrs.get("type") == "application/ld+json", "Keep content available without JavaScript"
            self.in_schema = True

    def handle_endtag(self, tag):
        if tag == "script":
            self.in_schema = False

    def handle_data(self, data):
        if self.in_schema:
            self.schema += data


root = Path(__file__).resolve().parents[1]
public = root / "website/public"
canonical = "https://agent-console.buhuipao.com/"
page = Page((public / "index.html").read_text())
ids = [attrs["id"] for _, attrs in page.tags if "id" in attrs]
assert len(ids) == len(set(ids)), "Duplicate HTML IDs"
assert sum(tag == "h1" for tag, _ in page.tags) == 1
assert ("html", {"lang": "en"}) in page.tags
meta = {a.get("name", a.get("property")): a.get("content") for tag, a in page.tags if tag == "meta"}
assert 50 <= len(meta["description"]) <= 170
assert "noindex" not in meta["robots"]
assert meta["og:url"] == canonical
assert meta["og:image"] == meta["twitter:image"]
assert any(tag == "link" and a.get("rel") == "canonical" and a["href"] == canonical for tag, a in page.tags)
assert any(a.get("href") == "mailto:support@buhuipao.com" for _, a in page.tags)

for tag, attrs in page.tags:
    if tag == "img":
        assert attrs.get("alt") and int(attrs["width"]) > 0 and int(attrs["height"]) > 0
    for key in ("href", "src", "poster"):
        if key not in attrs:
            continue
        url = urlsplit(attrs[key])
        if url.scheme or url.netloc:
            continue
        target = public / (url.path.lstrip("/") or "index.html")
        assert target.is_file(), f"Missing local target: {attrs[key]}"
        if url.fragment and target.name == "index.html":
            assert url.fragment in ids, f"Missing anchor: {attrs[key]}"

graph = json.loads(page.schema)["@graph"]
software = next(node for node in graph if "SoftwareApplication" in node["@type"])
assert software["url"] == canonical
assert software["maintainer"]["email"] == "support@buhuipao.com"
assert software["screenshot"] == meta["og:image"]
assert (public / urlsplit(software["screenshot"]).path.lstrip("/")).is_file()
locations = ElementTree.parse(public / "sitemap.xml").findall(".//{*}loc")
assert [loc.text for loc in locations] == [canonical]
assert f"Sitemap: {canonical}sitemap.xml" in (public / "robots.txt").read_text()
assert "noindex" in (public / "404.html").read_text()
config = json.loads((root / "website/wrangler.jsonc").read_text())
assert config["routes"] == [{"pattern": urlsplit(canonical).netloc, "custom_domain": True}]
assert config["assets"]["not_found_handling"] == "404-page"
assert not config["workers_dev"] and not config["preview_urls"]
print("Website checks passed: local links, assets, metadata, schema, sitemap, and domain.")
