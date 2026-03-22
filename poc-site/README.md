# Companion Website

This directory contains the lightweight static website for the Companion project.

It is separate from the ACP runtime work in the rest of the repo. Think of it as the landing page and messaging layer, not part of the gateway or harness stack.

## Stack

- plain HTML, CSS, and JavaScript
- no framework build step
- Vercel configuration in `vercel.json`

## Local Development

```sh
npm run dev
```

That serves the site on port `3000`.

You can also use any static file server:

```sh
python3 -m http.server 3000
```

## Files

- `index.html`: page content
- `styles.css`: site styling
- `script.js`: client-side interactions
- `vercel.json`: deployment and response-header configuration

## Deployment

The site can be deployed directly from this directory with Vercel:

```sh
vercel --prod
```

There is also a repository workflow under `.github/workflows/deploy.yml` for Vercel-based site deployment.

## Scope

The website is still a simple project-facing POC site. It does not mirror every infrastructure change in the repo, and it is intentionally much smaller in scope than the ACP gateway and harness work.
