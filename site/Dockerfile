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

# Stage 2 — minimal nginx runtime (official nginx:1.31.9-alpine,
# nginx 1.31.9). The official image compiles nginx from the upstream
# source tarball, so it carries the 2026-11-18 1.31.9 release with all
# five nginx2root CVE fixes, independent of the Alpine repo's own nginx
# package.
#
# Base pinning (ANT-277 digest convention, re-based 2026-11-20 by
# ANT-1284/ANT-1285): the runtime base is pinned by the tag-level
# (multi-arch index) digest in `.base-image-digest` (this directory)
# instead of the floating `nginx:1.31.9-alpine` tag, so a Docker Hub
# re-publish of the tag cannot swap the image underneath without a
# visible diff. The 2026-11-20 nginx2root decision (board/user, on
# ANT-1283/ANT-1284) moves the run stage to the official alpine image —
# the Jekyll build stage (`jekyll/jekyll:4`) is unchanged, and the
# retired ubuntu/nginx:1.26.3 (plucky) base and its in-image apt-pin
# path go away (plucky was excluded from Ubuntu USN-9159-1's nginx2root
# backport; no apt pinning is needed on this base).
#
# Unlike the retired ubuntu/nginx base, this alpine base DOES ship
# libexpat and libx11 (the latter family pulled in by
# nginx-module-image-filter), so the nightly Trivy scan may flag
# CVE-2026-77214 (libexpat) and CVE-2026-88806 (libx11) until the alpine
# base ships fixes — the entries are justified in vreckan-workflow
# `.trivy/site-trivyignore`.
FROM nginx:1.31.9-alpine@sha256:fb4658584bd9a916ffb6f6e365f97c25cfdb2923801f551ea08356e695f14ff7
# The alpine base's conf.d default is overwritten by our COPY below
# (the Ubuntu base needed a separate sites-enabled rm; alpine uses
# conf.d only).
COPY site/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=site-build /src/_site/ /usr/share/nginx/html/
EXPOSE 80
