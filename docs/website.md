# Website

The website is the static files in `site/`. GitHub Pages serves them from
`.github/workflows/pages.yml`, with GitHub Actions as the Pages source.

## Checks

Every pull request that touches `site/` runs `.github/scripts/check-site.py`, which fails if:

- a link or asset in an HTML or CSS file does not resolve to a file inside `site/`;
- a `#fragment` names no `id` on its page;
- a path starts with `/`, which breaks while the site lives under `/pinfold.sh/`. `404.html`
  is the exception: Pages serves it at any missing path, where only a root-relative path
  works, so it inlines its CSS and links home with `/`, which only works once the custom
  domain is live;
- anything but an `<a>` link or `<link rel="canonical">` points at another origin, so a page
  makes no third-party request.

It also prints the site's total size.

`docs/configuration.md` is the configuration reference's single source of truth.
`site/configuration.html` is built from it and is not committed: the workflow builds it from
the commit it checks or deploys, so two merges cannot leave a stale copy. Build it before you
check the site or open it locally, from the repository root:

```sh
python3 .github/scripts/build-config-page.py
python3 .github/scripts/check-site.py site
python3 -B -m unittest discover -s .github/scripts
```

## Deploy

A push to `main` that touches `site/` checks the site, builds `configuration.html`, uploads it with
`actions/upload-pages-artifact` and deploys it with `actions/deploy-pages` in the
`github-pages` environment. Only the deploy job can write, and only to Pages. Deploys queue
one at a time and none is cancelled midway. Without a `site/` directory the workflow checks
nothing and deploys nothing. It cannot be started by hand (#108): to redeploy, merge a change
to `site/`.

The site is served at <https://0xpiranhacodes.github.io/pinfold.sh/>.

## Custom domain

The site's domain will be `pinfold.sh`. The maintainer owns the domain and its DNS, and until
the DNS is live the site's address is the github.io URL above. To switch:

1. Verify the domain for the account or organization (Settings > Pages > Add a domain), so
   no other GitHub user can claim it if Pages is ever turned off.
2. At the DNS provider, add the apex and `www` records:

   ```text
   pinfold.sh.      A     185.199.108.153
   pinfold.sh.      A     185.199.109.153
   pinfold.sh.      A     185.199.110.153
   pinfold.sh.      A     185.199.111.153
   pinfold.sh.      AAAA  2606:50c0:8000::153
   pinfold.sh.      AAAA  2606:50c0:8001::153
   pinfold.sh.      AAAA  2606:50c0:8002::153
   pinfold.sh.      AAAA  2606:50c0:8003::153
   www.pinfold.sh.  CNAME 0xpiranhacodes.github.io.
   ```

3. Set the domain on the repository (Settings > Pages > Custom domain), or:

   ```sh
   gh api -X PUT repos/0xPiranhaCodes/pinfold.sh/pages -f cname=pinfold.sh
   ```

   Do this only once the DNS records resolve. `site/CNAME` records the domain, but with
   GitHub Actions as the source Pages ignores that file, so this step is what sets it.
4. Once GitHub has issued the certificate, which can take up to an hour, enforce HTTPS:

   ```sh
   gh api -X PUT repos/0xPiranhaCodes/pinfold.sh/pages -F https_enforced=true
   ```

The site uses relative paths, so it works both under `/pinfold.sh/` and at the root of a
custom domain.
