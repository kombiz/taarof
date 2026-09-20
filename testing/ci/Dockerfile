FROM mirror.gcr.io/library/archlinux:base

# Arch is rolling-release and does not support partial upgrades. Keep the base
# and build dependencies synchronized in one layer.
RUN pacman -Syu --noconfirm && pacman -S --noconfirm --needed \
    gcc \
    make \
    pkgconf \
    git \
    rust \
    nodejs \
    npm \
    mise \
    python \
    zsh \
    fish \
    jq \
    curl \
    rsync \
    ripgrep \
    diffutils \
    debugedit \
    fakeroot \
    gtk4 \
    libadwaita \
    gtksourceview5 \
    vte4 \
    tmux \
    && yes | pacman -Scc

# The Docker daemon resolves bind sources on the controller host, not inside
# the Actions runner container. Give the invoking runner UID a real identity in
# this image so HOME-cleared tests can resolve it without binding the host's
# unrelated /etc/passwd and /etc/group files.
ARG CI_UID=1000
ARG CI_GID=1000
RUN if ! getent group "$CI_GID" >/dev/null; then groupadd --gid "$CI_GID" ci; fi \
    && useradd --uid "$CI_UID" --gid "$CI_GID" \
        --home-dir /tmp/taarof-ci-home --no-create-home --shell /usr/bin/nologin ci
