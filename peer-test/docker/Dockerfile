# A throwaway peer box: real sshd, real filesystem, real PATH, real hostname.
#
# Arch base on purpose. The herdr binary under test is built on the host and
# mounted in dynamically linked, so the image's glibc must not be older than the
# host's (2.44 at the time of writing). A Debian or Ubuntu base fails at exec
# with a GLIBC_2.4x version error.
FROM archlinux:base

# iproute2 carries `tc`, which is how `boxes.sh netem` degrades the link
# between two boxes. It is named explicitly rather than relied on through the
# `base` metapackage, because that dependency is not ours to keep.
RUN pacman -Syu --noconfirm openssh iproute2 && pacman -Scc --noconfirm

# Named `herdr` to match the real test boxes, and because peer discovery looks
# for a binary literally called `herdr` on the far side.
RUN useradd -m -s /bin/bash herdr \
    && mkdir -p /home/herdr/.ssh /home/herdr/.local/bin /run/sshd /usr/local/bin \
    && chown -R herdr:herdr /home/herdr

# Key-only, no passwords, no root: a peer box should authenticate exactly the
# way a real one does.
RUN ssh-keygen -A \
    && printf '%s\n' \
        'PermitRootLogin no' \
        'PasswordAuthentication no' \
        'KbdInteractiveAuthentication no' \
        'PubkeyAuthentication yes' \
        'AcceptEnv HERDR_*' \
        > /etc/ssh/sshd_config.d/10-peer-box.conf

COPY --chown=herdr:herdr authorized_keys /home/herdr/.ssh/authorized_keys
RUN chmod 700 /home/herdr/.ssh && chmod 600 /home/herdr/.ssh/authorized_keys

# The binary is mounted, not baked, so a rebuild on the host is picked up by a
# container restart instead of an image rebuild.
ENV PATH=/home/herdr/.local/bin:/usr/local/bin:/usr/bin:/bin

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
