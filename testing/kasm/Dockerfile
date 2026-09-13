FROM lscr.io/linuxserver/webtop:arch-xfce

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:/lsiopy/bin:/command:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Arch does not support partial upgrades: refreshing package indexes and then
# installing current Node/Python against an older base glibc produces binaries
# that cannot start. Upgrade the base atomically with the build dependencies.
# Avoid base-devel group to prevent gcc-libs file conflicts with the base image.
RUN pacman -Syu --noconfirm && pacman -S --noconfirm --needed \
    gcc \
    make \
    pkgconf \
    git \
    rustup \
    nodejs \
    npm \
    python \
    zsh \
    fish \
    jq \
    curl \
    xterm \
    xdotool \
    scrot \
    ffmpeg \
    gtk4 \
    libadwaita \
    gtksourceview5 \
    vte4 \
    tmux \
    && yes | pacman -Scc \
    # The rolling nginx package can land ahead of Arch's binary fancyindex
    # module. Webtop only uses it for optional /files directory listings; a
    # mismatched module prevents the entire noVNC proxy from starting.
    && rm -f /etc/nginx/modules.d/fancy.conf \
    && sed -i '/fancyindex/d' /defaults/default.conf

# Initialize Rust stable toolchain for the runtime user and non-interactive
# `docker compose run` commands, not only the Docker build user's HOME.
RUN mkdir -p "$RUSTUP_HOME" "$CARGO_HOME" \
    && rustup default stable \
    && chmod -R a+rX "$RUSTUP_HOME" \
    && chmod -R a+rwX "$CARGO_HOME"
