FROM rust:1-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
    imagemagick libx11-6 libx11-xcb1 libxcursor1 libxi6 libxkbcommon-x11-0 libxkbcommon0 \
    libxrandr2 libwayland-client0 libwayland-cursor0 weston xclip xdotool xvfb >/dev/null
