"""Expands Freelens's PromQL templates into a fixture, without running JS.

Usage:
    python3 -I extract_freelens_promql.py FREELENS/packages/technical-features/prometheus/src \
        > freelens-promql.json

It reads each provider's TypeScript as text, finds every query template and
fills in fixed options, the way Freelens's getQuery would. The output lists
one entry per provider, category, query and node filter. `tests/promql.rs`
checks that the crate builds the same strings.

Freelens is MIT licensed; see NOTICE at the repository root.
"""

import json
import pathlib
import re
import sys

# The options every query is expanded with. tests/promql.rs builds the same.
OPTS = {
    "mountpoints": "/|/local",
    "pods": "web-1|web-2",
    "namespace": "shop",
    "selector": "pod, namespace",
    "pvc": "data-0",
    "ingress": "web",
}
NODES = "node-a|node-b"
CATEGORIES = {"cluster", "nodes", "pods", "pvc", "ingress"}


def js_string(literal):
    """The value of a double-quoted JS string literal's contents."""
    return literal.encode().decode("unicode_escape")


def node_filter(opts, label):
    # provider.ts: nodeFilter
    return f'{{{label}=~"{opts["nodes"]}"}}' if opts.get("nodes") else ""


def node_filter_additive(opts, label):
    # provider.ts: nodeFilterAdditive
    return f',{label}=~"{opts["nodes"]}"' if opts.get("nodes") else ""


class Expander:
    def __init__(self, src):
        self.src = src
        provider_ts = (src / "provider.ts").read_text()
        self.bytes_sent = re.search(
            r"function bytesSent\(.*?\): string \{\s*return `(.*?)`;", provider_ts, re.S
        ).group(1)
        operator_ts = (src / "operator-provider.injectable.ts").read_text()
        self.node_join = re.search(
            r"const nodeExporterJoin = .*?opts\.nodes\s*\?\s*`(.*?)`\s*:\s*\"\"", operator_ts, re.S
        ).group(1)

    def expr(self, text, env):
        text = text.strip()
        if text in env:
            return env[text]
        if text.startswith("opts."):
            return env["opts"][text[5:]]
        m = re.fullmatch(r'nodeFilter\(opts, "(\w+)"\)', text)
        if m:
            return node_filter(env["opts"], m.group(1))
        m = re.fullmatch(r'nodeFilterAdditive\(opts, "(\w+)"\)', text)
        if m:
            return node_filter_additive(env["opts"], m.group(1))
        if text == "nodeExporterJoin(opts)":
            return self.template(self.node_join, env) if env["opts"].get("nodes") else ""
        raise ValueError(f"unknown expression: {text}")

    def template(self, body, env):
        return re.sub(r"\$\{([^}]*)\}", lambda m: self.expr(m.group(1), env), body)

    def call_bytes_sent(self, args, env):
        values = {}
        for key, value in re.findall(r"(\w+)(?::\s*([^,\n]+))?,", args):
            value = value.strip() if value else key
            if value.startswith('"'):
                values[key] = js_string(value[1:-1])
            else:
                values[key] = self.expr(value, env)
        return self.template(self.bytes_sent, {**env, **values})

    def queries(self, function_body, rate, opts):
        """Yields (category, query name, promql) for one getQuery function."""
        category = None
        lines = function_body.splitlines()
        i = 0
        while i < len(lines):
            line = lines[i].strip()
            m = re.fullmatch(r'case "(\w+)":( \{)?', line)
            if m and m.group(1) in CATEGORIES:
                category = m.group(1)
            elif m:
                name = m.group(1)
                env = {"rateAccuracy": rate, "opts": opts}
                i += 1
                while True:
                    line = lines[i].strip()
                    const = re.fullmatch(r"const (\w+) = (.*);", line)
                    if const:
                        env[const.group(1)] = self.expr(const.group(2), env)
                    elif line.startswith("return `"):
                        yield category, name, self.template(line[len("return `") : -2], env)
                        break
                    elif line.startswith("return bytesSent({"):
                        args = []
                        i += 1
                        while not lines[i].strip().startswith("});"):
                            args.append(lines[i].strip())
                            i += 1
                        yield category, name, self.call_bytes_sent("\n".join(args) + "\n", env)
                        break
                    i += 1
            i += 1


def main():
    src = pathlib.Path(sys.argv[1])
    expander = Expander(src)
    functions = {}
    for path in sorted(src.glob("*-provider.injectable.ts")):
        text = path.read_text()
        for m in re.finditer(r"export const (get\w+LikeQueryFor) =(.*?)\n  \};\n", text, re.S):
            functions[m.group(1)] = m.group(2)
    entries = []
    for path in sorted(src.glob("*-provider.injectable.ts")):
        text = path.read_text()
        kind = re.search(r'kind: "(\w+)"', text).group(1)
        m = re.search(r'getQuery: (get\w+LikeQueryFor)\(\{ rateAccuracy: "(\w+)" \}\)', text)
        function, rate = m.group(1), m.group(2)
        for nodes in (None, NODES):
            opts = dict(OPTS)
            if nodes:
                opts["nodes"] = nodes
            for category, name, promql in expander.queries(functions[function], rate, opts):
                if nodes and category != "cluster":
                    continue
                entries.append(
                    {
                        "provider": kind,
                        "category": category,
                        "query": name,
                        "nodes": nodes is not None,
                        "promql": promql,
                    }
                )
    json.dump(entries, sys.stdout, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
