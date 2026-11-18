#!/usr/bin/env bash
# Builds the running example of the thesis as a git repository with three
# branches developed "concurrently" from the same base:
#
#   agent-a  changes the CONTRACT of notify.email.send_email (adds user_id)
#   agent-b  adds auth/reset.py, which calls send_email the OLD way
#   agent-c  changes only the BODY of notify.email.format_subject
#
# All pairwise git merges are clean, yet merging agent-a and agent-b breaks the
# program. Usage: setup.sh <target-dir>
set -euo pipefail

dir="${1:?usage: setup.sh <target-dir>}"
mkdir -p "$dir"
cd "$dir"
git init -q -b main
git config user.email "demo@example.com"
git config user.name "demo"

mkdir -p notify auth
: > notify/__init__.py
: > auth/__init__.py

cat > notify/email.py <<'PY'
import smtplib


def format_subject(subject):
    return subject.strip()


def send_email(to, body):
    smtp = smtplib.SMTP("localhost")
    smtp.sendmail("noreply@example.com", [to], body)
PY

cat > auth/tokens.py <<'PY'
import secrets


def issue(user):
    return f"{user.id}-{secrets.token_hex(8)}"
PY

cat > app.py <<'PY'
from notify.email import send_email


def welcome(user):
    send_email(user.email, "Welcome!")
PY

git add -A
git commit -q -m "base"

# --- agent A: rate-limit outgoing e-mails per user (contract change) ---------
git checkout -q -b agent-a main
cat > notify/email.py <<'PY'
import smtplib


def format_subject(subject):
    return subject.strip()


def send_email(to, body, *, user_id):
    if SENT.get(user_id, 0) >= LIMIT:
        raise RuntimeError(f"rate limit exceeded for user {user_id}")
    SENT[user_id] = SENT.get(user_id, 0) + 1
    smtp = smtplib.SMTP("localhost")
    smtp.sendmail("noreply@example.com", [to], body)


LIMIT = 5
SENT = {}
PY
cat > app.py <<'PY'
from notify.email import send_email


def welcome(user):
    send_email(user.email, "Welcome!", user_id=user.id)
PY
git commit -q -am "agent A: rate-limit e-mails per user"

# --- agent B: password-reset flow (new code that calls send_email) -----------
git checkout -q -b agent-b main
cat > auth/reset.py <<'PY'
from auth import tokens
from notify.email import send_email


def reset_msg(token):
    return f"Reset your password: https://example.com/reset/{token}"


def request_reset(user):
    token = tokens.issue(user)
    send_email(user.email, reset_msg(token))
PY
git add -A
git commit -q -m "agent B: password reset"

# --- agent C: nicer subjects (body-only change in agent A's file) ------------
git checkout -q -b agent-c main
cat > notify/email.py <<'PY'
import smtplib


def format_subject(subject):
    return " ".join(subject.split()).capitalize()


def send_email(to, body):
    smtp = smtplib.SMTP("localhost")
    smtp.sendmail("noreply@example.com", [to], body)
PY
git commit -q -am "agent C: normalise subjects"

git checkout -q main
echo "running example created in $(pwd): branches agent-a, agent-b, agent-c"
