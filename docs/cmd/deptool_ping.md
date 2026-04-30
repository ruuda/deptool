# deptool ping

    deptool ping [--limit <hosts>]... [--] [<dir>]

## Description

Measure round-trip latencies to every host in the cluster. This measures the
latency over the <abbr>SSH</abbr> connection, using the same agent protocol
that the other commands use. It’s not an <abbr>ICMP</abbr> ping, which means
it works even when <abbr>ICMP</abbr> ping is blocked. On the flip side, because
pings travel over an <abbr>SSH</abbr> connection, this tool cannot measure
packet loss.

See also [`deptool deploy`](deptool_deploy.md) for details about the cluster
config tree directory `<dir>`. As with other commands, ping defaults to the
last-used cluster when you omit it.

## Output

The command outputs live statistics as it collects data, up to 150 pings per
host:

    a.example.com:   5.9 ms |   6.3 ms |   7.9 ms  (min/p50/p95 rtt, n=150)
    b.example.com:   6.6 ms |   7.8 ms |  14.5 ms  (min/p50/p95 rtt, n=150)
    c.example.com:  13.9 ms |  14.4 ms |  16.5 ms  (min/p50/p95 rtt, n=150)
    d.example.com:  97.2 ms |  98.1 ms |  99.6 ms  (min/p50/p95 rtt, n=150)

It prints the minimum observed round-trip time in milliseconds (first column),
the 50th percentile (second column), and 95th percentile (third column).

## Options

### `--limit <hosts>`

Limit the hosts to connect to. Can be provided multiple times, and supports a
comma-separated list of hosts too. For example, in a cluster with hosts `web1`
through `web5`, passing `--limit web1,web2 --limit web3` would exclude `web4`
and `web5` from the measurement.

### `--store`

Path to the local [store](../store.md), by default `.deptool`.
