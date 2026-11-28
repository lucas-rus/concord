# The always-online concord-team relay + Discord bot (Railway, Fly, any Docker host).
FROM python:3.12-slim
RUN pip install --no-cache-dir pynacl==1.5.0
WORKDIR /app
COPY concord_team.py /app/concord_team.py
ENV PYTHONUNBUFFERED=1
# Railway sets PORT; mount a volume at /data so work and messages survive restarts.
CMD ["python", "/app/concord_team.py", "serve", "--data", "/data"]
