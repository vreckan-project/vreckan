# Vreckan site container — multi-stage build.
# Build context: repository root.
#   docker build -f site/Dockerfile .
# Stage 1 builds the Jekyll site; stage 2 is a minimal nginx runtime
# with no Ruby or build tools.

# syntax=docker/dockerfile:1

# Stage 1 — build the Jekyll site
FROM jekyll/jekyll:4 AS site-build
WORKDIR /src
COPY site/ ./
RUN bundle install --quiet && jekyll build --quiet

# Stage 2 — minimal nginx runtime.
#
# Base pinning (ANT-315, following the ANT-277 digest convention): the
# runtime base is pinned by the tag-level digest in `.base-image-digest`
# (this directory) instead of the floating `nginx:1.27-alpine` tag, so a
# Docker Hub re-publish of the tag cannot swap the alpine underneath the
# image without a visible diff. Bumped 1.27 (alpine 3.21) -> 1.30 (alpine
# 3.24) to clear the nightly Trivy findings (openssl, libexpat, zlib,
# c-ares, libpng, libxml2, musl, nghttp2, tiff). One known-accepted
# finding rides in the scan-rust allowlist until alpine 3.25 ships
# libexpat 2.9.0: CVE-2026-77214 (see .trivy/site-trivyignore in
# vreckan-workflow).
FROM nginx:1.30-alpine@sha256:970364228b1e4f932abc22d321932816782cca8a20b85a77552a7cfe9acfb3b5
COPY site/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=site-build /src/_site/ /usr/share/nginx/html/
EXPOSE 80
