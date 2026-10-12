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

# Stage 2 — minimal nginx runtime
FROM nginx:1.27-alpine
COPY site/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=site-build /src/_site/ /usr/share/nginx/html/
EXPOSE 80
