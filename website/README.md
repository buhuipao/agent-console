# Agent Console website

Public URL: <https://agent-console.buhuipao.com/>. Contact:
<support@buhuipao.com>.

`public/` is a static HTML and CSS site. It has no build step or client-side
JavaScript. Cloudflare Workers serves only this directory. The product's local
browser app remains in `assets/web/` and is not part of this deployment.

## Preview and check

Run from the repository root with Python 3:

```sh
python3 tests/website.py
python3 -m http.server 4173 --bind 127.0.0.1 --directory website/public
```

Open <http://127.0.0.1:4173/>. Check desktop and narrow phone widths, keyboard
navigation, the FAQ disclosures, video controls, and the download/contact links.
The Python server does not apply Cloudflare headers or custom 404 handling.

## Deploy

Use Node.js 22+ and a Cloudflare login with Workers access to the account that
owns `buhuipao.com`. Wrangler 4.134.0 was used for the initial deployment.

```sh
npx wrangler@4.134.0 login
npx wrangler@4.134.0 deploy --dry-run --config website/wrangler.jsonc
npx wrangler@4.134.0 deploy --config website/wrangler.jsonc
```

The custom-domain route creates the Cloudflare DNS record and certificate.
`workers.dev` and preview URLs are disabled so the public site has one host.
Keep credentials outside the repository. CI checks the site; deployment uses
the command above after the reviewed changes reach `main`.

After each deployment, verify the public HTTPS page, the HTTP-to-HTTPS redirect,
`/robots.txt`, `/sitemap.xml`, the screenshot, and the video. An unknown path must
return HTTP 404. Check that the served HTML and CSS match the deployed commit.
The manual CI run performs these live checks from a GitHub-hosted runner:

```sh
gh workflow run ci.yml --ref main --repo buhuipao/agent-console
```

It compares all public assets with the checked-out commit and tests video range
requests. Normal push and pull-request runs only check the local site files.

To restore an earlier site, deploy its `website/` directory from a clean
worktree; this also restores its static assets.

## Content and search

The README, changelog, and provider compatibility guide are the sources for
product claims. Check both current source and public releases before changing
availability or install copy. Sample sessions in the images and video are
identified as demo data. `assets/dashboard.png` is copied from
`docs/assets/dashboard.png`; `assets/demo.mp4` is the silent recording from
`docs/assets/demo.gif`, converted to H.264 for browser controls and playback.

All content is present in the initial HTML. The page has a canonical URL,
descriptive metadata, social preview metadata, matching SoftwareApplication /
SoftwareSourceCode structured data, a sitemap, crawl rules, semantic headings,
and visible answers with links to primary product documentation. FAQ markup
does not claim eligibility for a search rich result. No analytics is installed.

The visual reference is [Laper](https://laper.ai/): light background, restrained
green accents, a product preview, and sections built around the workflow.
Product copy and media are Agent Console's own.

Reference guidance checked on 2026-09-18:

- [Google: helpful content](https://developers.google.com/search/docs/fundamentals/creating-helpful-content)
- [Google: AI features and websites](https://developers.google.com/search/docs/appearance/ai-features)
- [Cloudflare: static assets](https://developers.cloudflare.com/workers/static-assets/)
- [Cloudflare: custom domains](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/)

Production reachability, search indexing, rankings, and AI citations are
separate checks. This setup does not prove indexing or citations. Check search
discovery after seven days and review available search data after 28 days;
Search Console and Bing reports remain unverified until access is available.
