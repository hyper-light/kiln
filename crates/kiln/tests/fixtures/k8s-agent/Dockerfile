# The agents' image for k8s_agent_live (docs/agentic.md W10): kiln at /usr/local/bin/kiln (the init container copies it
# into the pod) and the stand-in harness as `claude`, acting on each message by `act`. Built from a folder holding this
# file, `act`, `claude`, the stand-in (`fake-claude`, tests/fixtures/fake-claude/claude) and a Linux `kiln`:
#   docker build -t k90-close-agent:live <that folder>
# archlinux: the C library of the Arch-family host kiln is built on, so the host's build runs in it.
FROM archlinux:base
# A full upgrade, as Arch asks (a partial one meets its packages' splits: openssl-3, 2026), the image's own files overwritten.
RUN pacman -Syu --noconfirm --needed --overwrite '*' python git && pacman -Scc --noconfirm
COPY kiln /usr/local/bin/kiln
COPY fake-claude /opt/fake-claude/claude
COPY claude act /usr/local/bin/
RUN chmod 0755 /usr/local/bin/kiln /opt/fake-claude/claude /usr/local/bin/claude /usr/local/bin/act
# The pod runs as the folder's owner, who has no home in the image.
ENV HOME=/tmp
