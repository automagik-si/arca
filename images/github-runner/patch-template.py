#!/usr/bin/env python3
"""Turn GitHub's Packer build file for an Ubuntu runner image into the one build.sh runs.

    patch-template.py <their build file> <output file> <strict|record>

GitHub's template is run as it is, except for these changes, each of which must find exactly what it expects in the
file (or the build stops, naming the change, so a new release that moved things is looked at rather than built
wrongly). The same list is in the published difference (README.md, "What we change"):

1. The source: their `azure-arm` builder becomes Packer's `null` builder (source.arca.pkr.hcl), which reaches the Incus
   virtual machine build.sh started over SSH.
2. The last provisioner, `waagent -force -deprovision+user`, is left out: there is no Azure agent. build.sh's
   vm/finalize.sh does that clean-up instead (it removes the build user, cloud-init's state and the logs).
3. Only in record mode (the default): GitHub's image tests still run, at every place their scripts run them, but a
   failed test is written to a list and the build goes on, so one failure does not cost a whole build. The list is
   published with the image. In strict mode a failed test stops the build, as on GitHub.
"""
import re
import sys

RECORD_LOG = "/usr/local/lib/arca-image/test-failures.log"


def blocks(text):
    """(start, end) of every top-level `provisioner "..." { ... }` block inside the build block, braces in strings skipped."""
    found = []
    for match in re.finditer(r'(?m)^[ \t]*provisioner "', text):
        start = match.start()
        i = text.index("{", match.end())
        depth, in_string = 0, False
        while i < len(text):
            c = text[i]
            if in_string:
                if c == "\\":
                    i += 1
                elif c == '"':
                    in_string = False
            elif c == '"':
                in_string = True
            elif c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    end = i + 1
                    if end < len(text) and text[end] == "\n":
                        end += 1
                    found.append((start, end))
                    break
            i += 1
        else:
            sys.exit("patch-template: a provisioner block does not close")
    return found


def only_one(what, items):
    if len(items) != 1:
        sys.exit(f"patch-template: change '{what}' expected exactly one match in GitHub's template and found "
                 f"{len(items)}. Their template changed: look at it and update patch-template.py")
    return items[0]


def main():
    if len(sys.argv) != 4 or sys.argv[3] not in ("strict", "record"):
        sys.exit(__doc__)
    source, output, mode = sys.argv[1:]
    text = open(source, encoding="utf-8").read()

    # 1. The source.
    old = 'sources = ["source.azure-arm.image"]'
    only_one("source", [m.start() for m in re.finditer(re.escape(old), text)])
    text = text.replace(old, 'sources = ["source.null.arca"]')

    # 2. The Azure agent's deprovision.
    start, end = only_one("waagent", [b for b in blocks(text) if "waagent -force -deprovision+user" in text[b[0]:b[1]]])
    text = text[:start] + text[end:]

    if mode == "record":
        # 3a. invoke_tests (which every install script calls) records instead of stopping. configure-environment.sh
        # makes the link to GitHub's helper; this provisioner, right after it, points the link at our wrapper.
        start, end = only_one("record tests", [b for b in blocks(text)
                                               if "scripts/build/configure-environment.sh" in text[b[0]:b[1]]])
        ours = (
            '  provisioner "shell" {\n'
            '    execute_command = "sudo sh -c \'{{ .Vars }} {{ .Path }}\'"\n'
            '    inline          = ["ln -sf /usr/local/lib/arca-image/invoke-tests-record.sh /usr/local/bin/invoke_tests"]\n'
            '  }\n\n'
        )
        text = text[:end] + "\n" + ours.rstrip("\n") + "\n" + text[end:]
        # 3b. The final run of every test.
        old = "/tests/RunAll-Tests.ps1 -OutputDirectory ${var.image_folder}\""
        only_one("record final tests", [m.start() for m in re.finditer(re.escape(old), text)])
        text = text.replace(old, "/tests/RunAll-Tests.ps1 -OutputDirectory ${var.image_folder} || "
                                 f"echo RunAll-Tests >> {RECORD_LOG}\"")

    header = (f"# Made by arca images/github-runner/patch-template.py ({mode} mode) from GitHub's {source.rsplit('/', 1)[-1]}.\n"
              "# Do not edit: build.sh writes it again for every build.\n")
    open(output, "w", encoding="utf-8").write(header + text)


if __name__ == "__main__":
    main()
