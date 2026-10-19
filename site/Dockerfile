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

# Stage 2 — minimal nginx runtime (Ubuntu 25.04 "plucky", nginx 1.26.3).
#
# Base pinning (ANT-277 digest convention, re-based in ANT-317): the
# runtime base is pinned by the tag-level digest in `.base-image-digest`
# (this directory) instead of the floating `ubuntu/nginx` tag, so a
# Docker Hub re-publish of the tag cannot swap the image underneath
# without a visible diff. ANT-317 swapped the base from nginx:1.30-alpine
# (alpine 3.24) to ubuntu/nginx: Ubuntu's nginx build installs neither
# libexpat nor libx11, so the allowlisted CVE-2026-77214 (libexpat) and
# CVE-2026-88806 (libx11) are structurally gone instead of waiting on the
# alpine 3.24 image rebuild (vreckan-workflow `.trivy/site-trivyignore`
# is emptied in the companion change).
#
# gpgv (ANT-317 -> ANT-358): the base ships gpgv
# 2.4.4-2ubuntu23.1 with CVE-2025-68973 (HIGH). ANT-317 patched that at
# build time with a pinned `apt-get install gpgv=2.4.4-2ubuntu23.2`;
# ANT-358 deletes that RUN ahead of the ubuntu/nginx re-publish that
# bakes in the fixed gpgv, and re-pins `.base-image-digest` when the
# `:latest` tag digest moves past the current pin. While the pin still
# predates the re-publish, that window is covered by the temporary
# CVE-2025-68973 entry in vreckan-workflow `.trivy/site-trivyignore`.
FROM ubuntu/nginx@sha256:b50aa594cd940fa459144efa129dc21466f7d56d015993914f41f3eb983c893b
# Ubuntu's nginx package ships a default site under sites-enabled/
# (alpine's uses conf.d only); without the rm the package default server
# can conflict with ours on :80.
RUN rm -f /etc/nginx/sites-enabled/default
COPY site/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=site-build /src/_site/ /usr/share/nginx/html/
EXPOSE 80
