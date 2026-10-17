# db.center

Everything in this folder is published to https://db.center by Cloudflare Workers
on every push to `main`, as static assets: `wrangler.jsonc` at the repository root
says where they are.
Put nothing here that should not be public.

To look at it locally: `python3 -m http.server -d dbcenterwebsite 8000`.

The older pitch deck and promo cards are in `marketing/`. They predate 0.4 and
are not published.
