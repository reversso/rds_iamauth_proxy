# rds-iamauth-proxy

## Configuration
`rds-proxy` lets you make use of IAM-based authentication to
AWS RDS instances from tools that don't natively support
that method of authentication.

To use it, set environment variables that point to the desired RDS instance.
When you run the proxy, it uses the standard methods of picking up an
AWS credential (e.g. credentials file, environment variables, etc.).
It also uses the AWS SDK's standard region resolution, so set `AWS_REGION`
or `AWS_DEFAULT_REGION` outside ECS. ECS sets `AWS_REGION` automatically.

Optionally, you can point the proxy at a different endpoint to make use
of something like an SSH tunnel to a bastion host.

Environment variables:

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `DB_HOST` | Yes | | Real RDS hostname used for IAM signing and TLS. |
| `DB_PORT` | No | `5432` | Real RDS port. |
| `CONNECT_HOST` | No | `DB_HOST` | Host the proxy opens a TCP connection to. Use this for SSH tunnels. |
| `CONNECT_PORT` | No | `DB_PORT` | Port the proxy opens a TCP connection to. |
| `LISTEN_ADDR` | No | `127.0.0.1:5435` | Address the proxy listens on. |
| `PASSWORD_CACHE_TTL_SECS` | No | `600` | How long generated IAM auth passwords are reused. |

Direct connection:

```sh
export AWS_REGION=us-east-1
export DB_HOST=db.abcdef.us-east-1.rds.amazonaws.com
rds_proxy
```

SSH tunnel:

```sh
ssh -L 55432:db.abcdef.us-east-1.rds.amazonaws.com:5432 bastion.example.com

export AWS_REGION=us-east-1
export DB_HOST=db.abcdef.us-east-1.rds.amazonaws.com
export CONNECT_HOST=localhost
export CONNECT_PORT=55432
rds_proxy
```

ECS task definitions should set at least `DB_HOST`. ECS provides
`AWS_REGION`; credentials should come from the task role.

## Install & Usage

Installation: `cargo install rds_proxy`

Usage: `rds_proxy`

You can override the listen address with `--listen`:

```sh
rds_proxy --listen 0.0.0.0:5435
```

Upon success the proxy will be available for connections on `127.0.0.1:5435`.
The connection string passed to the tool making use of the proxy can
include any relevant username that the backend RDS instance is expecting. The
password field is ignored.

## Development

To build the docker container locally,
```sh
docker build -t rds_proxy:test .
```

To run it locally with Docker:

```sh
DB_HOST=db.abcdef.us-east-1.rds.amazonaws.com \
AWS_REGION=us-east-1 \
./run_docker.sh
```

## Notes

If installation fails with `error: failed to download zeroize v1.4.1` — please ensure `cargo` is up to date and try again.
