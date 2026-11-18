# GitLab CE migration to an Arca service

**Not yet run on a live node.** Template and runbook only; no migration has been performed.
The first step is an empty-service rehearsal. Do not move old data until readiness, a repository write and
a backup/restore round trip all pass.
Only the owner or the operator they authorize runs these commands, after review and approval of host changes.

This procedure is for a single-node Omnibus CE installation, using its bundled PostgreSQL and local repository
storage, on **19.4.8-ce.0 on both sides**. Distributed databases, Geo, external Redis, object storage, registry
metadata databases and custom storage paths need a separately checked procedure before proceeding. A backup
does not make these configurations portable automatically. Keep the original external URL for sign-ins and
WebAuthn. Replace every `<placeholder>` locally; never put populated commands or configuration in a repository.
The old-side commands assume an Incus container with a dpkg-managed package. **Not checked:** the source runtime;
if it differs, verify equivalent exec, version and recovery-copy commands before proceeding.

Sources checked: GitLab's [Back up GitLab](https://docs.gitlab.com/administration/backup_restore/backup_gitlab/),
[Restore GitLab](https://docs.gitlab.com/administration/backup_restore/restore_gitlab/) (including its Docker
section), [Migrate to a new server](https://docs.gitlab.com/administration/backup_restore/migrate_to_new_server/),
[Configure GitLab running in a Docker container](https://docs.gitlab.com/install/docker/configuration/),
[Configure SSL for a Linux package installation](https://docs.gitlab.com/omnibus/settings/ssl/),
[Health check](https://docs.gitlab.com/administration/monitoring/health_check/) and
[GitLab installation requirements](https://docs.gitlab.com/install/requirements/). These are the current pages
covering 19.x, not an immutable 19.4 documentation snapshot. Recheck them before execution.
Arca commands and limitations were checked against this revision's `service.rs`, `handlers.rs`,
`service_update.rs`, `schema/arca.openrpc.json` and [OPERATING.md](OPERATING.md).

**Not checked on a live node:** every execution below; GitLab init as PID 1 under Incus; symlinks with Omnibus
reconfigure/restore; restart and rebuild persistence; `/dev/shm` capacity (the Docker guide asks for 256MiB);
kernel-setting restrictions; first-start duration and peak memory; this installation's disk requirements;
edge, DNS, SSH, runners and mail. The publisher's `/assets/gitlab.rb` disables init detection and kernel-parameter
modification, but that is source evidence, not proof that the image boots under Arca. Template fields cannot
declare shared-memory size or an HTTP readiness check. A successful Arca process check is insufficient.

1. **Create an empty service and prove readiness before touching the old data.**

   On the new node (`ssh '<node>'`), use a private, unrecorded Bash terminal. Disable tracing and avoid recording
   terminal output during secret handling. Install the templates from the build containing `gitlab`:

   ```sh
   set +x
   set -o pipefail
   umask 077
   arca templates install --config '<config>'
   arca call action_propose --config '<config>' --args '{"action":"service_create","args":{"name":"<new-service>","template":"gitlab","start":false}}'
   ```

   The owner approves this proposal on Arca's approval page. Check the resulting `service_status` shows the
   pinned image and `state: "stopped"`; record the returned instance, project and data volume for later commands:

   ```sh
   arca call service_status --config '<config>' --args '{"name":"<new-service>"}'
   ```

   Before its first start, provide the owner's URL using `service_secret_set.value`, with key
   `GITLAB_EXTERNAL_URL`. OCI containers have no create-time `settings` or URL argument. There is no default in
   this template. The value must be an absolute http or https URL; use the existing https origin for this
   migration. This direct secret verb requires a doctor token; it cannot be proposed. Keep its argument file
   private and outside the checkout. This example prompts without echo, uses a mode-0600 file in the user's
   runtime directory, and removes it after the call:

   ```sh
   secret_args=$(mktemp "/run/user/$(id -u)/gitlab-url.XXXXXX")
   python3 - "$secret_args" <<'PY'
   import getpass, json, sys
   with open(sys.argv[1], 'w') as output:
       json.dump({'name': '<new-service>', 'key': 'GITLAB_EXTERNAL_URL',
                  'value': getpass.getpass('External GitLab URL: ')}, output)
   PY
   arca call service_secret_set --config '<config>' --token-file '<doctor-token-file>' --args-file "$secret_args"
   rm -f "$secret_args"
   unset secret_args
   arca call action_propose --config '<config>' --args '{"action":"instance_start","args":{"project":"<new-project>","instance":"<new-container>"}}'
   ```

   The owner approves the start. Arca has generated `GITLAB_ROOT_PASSWORD`; do not print it. That password
   initializes only a new installation; restoring a backup restores the old accounts and passwords.
   `service_logs` masks the external URL as `[secret]`. A missing or invalid URL makes the entrypoint exit with
   one plain instruction instead of starting GitLab. Revoke any temporary doctor token after provisioning.

   Run these checks on the new node; repeat the readiness request manually while first boot completes.
   Every HTTP check below requires status **200 exactly** and follows no redirects. `curl --fail` alone accepts
   3xx responses; a redirect is a failed check even when it points back to the requested URL. Stop on any
   nonzero command exit, TLS/connection error or status other than 200 before proceeding:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- dpkg-query -W gitlab-ce
   sudo incus exec --project '<new-project>' '<new-container>' -- ps -p 1 -o args=
   sudo incus exec --project '<new-project>' '<new-container>' -- df -h /dev/shm /data /
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl status
   readiness_status=$(sudo incus exec --project '<new-project>' '<new-container>' -- \
     curl --silent --show-error --max-time 10 --max-redirs 0 --output /dev/null \
     --write-out '%{http_code}' 'http://localhost/-/readiness?all=1') && test "$readiness_status" = 200
   sudo incus exec --project '<new-project>' '<new-container>' -- readlink -f /etc/gitlab /var/opt/gitlab /var/log/gitlab
   arca call service_status --config '<config>' --args '{"name":"<new-service>"}'
   ```

   Require `gitlab-ce 19.4.8-ce.0`, the publisher's init as PID 1, all expected services up, HTTP 200 with passing
   readiness checks, and the three paths resolving to `/data/config`, `/data/data`, `/data/logs`. Confirm at
   least 256MiB shared memory, no kernel-setting errors, acceptable first-start time and memory, and the expected
   data device. If any check fails, stop here and record it as **not checked/failed**; do not move the old data.
   A private-port SSH variant, if required, must be selected and rehearsed before this create (step 9).

   **Rehearse a repository write and a backup/restore round trip now, using only disposable data.** Nothing from
   the old instance may be copied until this succeeds. Keep the original service and shared DNS unchanged.
   Stage only step 8's approved link/route and client-only name resolution on an **isolated rehearsal edge** for
   the existing external hostname; never replace a route serving the old instance. Verify the edge's sign-in
   request from step 8 returns exactly 200, without redirects. **Not checked:** isolated client access and fresh
   administrator credential handling; arrange these privately with the owner or stop here. Never print the
   generated root password or put credentials in a URL, command argument or shell history.

   On that client, sign in to the empty service and create a private blank project, `<rehearsal-project>`, in
   `<rehearsal-namespace>`, without a README. Confirm the project page shows an empty repository on this service.
   Use prompted credentials or the owner's credential manager for Git. From a private client directory:

   ```sh
   rehearsal_dir=$(mktemp -d)
   git -C "$rehearsal_dir" init -b main
   printf '%s\n' 'rehearsal-only' > "$rehearsal_dir/README.md"
   git -C "$rehearsal_dir" add README.md
   git -C "$rehearsal_dir" commit -m 'Rehearse repository storage before migration'
   git -C "$rehearsal_dir" remote add origin 'https://<external-hostname>/<rehearsal-namespace>/<rehearsal-project>.git'
   git -C "$rehearsal_dir" -c http.followRedirects=false push -u origin main
   rehearsal_head=$(git -C "$rehearsal_dir" rev-parse HEAD)
   test "$(git -C "$rehearsal_dir" -c http.followRedirects=false ls-remote origin refs/heads/main | cut -f1)" = "$rehearsal_head"
   ```

   Require push exit 0, the same commit on the project page, and a matching remote branch hash. Keep this client
   shell and `rehearsal_head` for the restore check. No runners, mail, integrations or other users may write to
   this rehearsal service. On the new node, stop the application writers and create the rehearsal backup:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl stop puma
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl stop sidekiq
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl status
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-backup create
   sudo incus exec --project '<new-project>' '<new-container>' -- find /var/opt/gitlab/backups -maxdepth 1 -name '*_gitlab_backup.tar' -printf '%f %s bytes\n'
   sudo incus exec --project '<new-project>' '<new-container>' -- tar -tf '/var/opt/gitlab/backups/<rehearsal-backup-id>_gitlab_backup.tar' > /dev/null
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl start
   ```

   Require Puma and Sidekiq down while PostgreSQL, Redis and Gitaly remain up for backup, a successful backup
   exit, and a new nonzero archive whose listing succeeds. Record its ID as `<rehearsal-backup-id>`, distinct
   from the later real `<backup-id>`. Check free backup-directory space is at least three archive sizes before
   restoring, with additional room for expansion. Keep `/etc/gitlab` and its secrets unchanged for this round trip.
   After starting, repeat the exact readiness and staged-edge checks above; both must return 200. On the client,
   push a second, deliberately unbacked-up commit so a successful restore must visibly roll it back:

   ```sh
   printf '%s\n' 'after-rehearsal-backup' >> "$rehearsal_dir/README.md"
   git -C "$rehearsal_dir" add README.md
   git -C "$rehearsal_dir" commit -m 'Verify rehearsal restore rolls back later writes'
   git -C "$rehearsal_dir" -c http.followRedirects=false push origin main
   test "$(git -C "$rehearsal_dir" -c http.followRedirects=false ls-remote origin refs/heads/main | cut -f1)" = "$(git -C "$rehearsal_dir" rev-parse HEAD)"
   test "$(git -C "$rehearsal_dir" rev-parse HEAD)" != "$rehearsal_head"
   ```

   Require the second push and both hash checks to succeed. On the new node, restore **only the rehearsal
   archive into this still-disposable service**. Answer the restore confirmations and require each command's
   exit 0; this intentionally replaces the rehearsal database and repository, never the old installation:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl stop puma
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl stop sidekiq
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl status
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-backup restore 'BACKUP=<rehearsal-backup-id>'
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-psql -d gitlabhq_production -c 'SET statement_timeout = 0; ANALYZE VERBOSE;'
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl reconfigure
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl restart
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:check SANITIZE=true
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:doctor:secrets
   sudo incus exec --project '<new-project>' '<new-container>' -- readlink -f /etc/gitlab /var/opt/gitlab /var/log/gitlab
   ```

   Before restore, require Puma and Sidekiq down and the backup dependencies up. Afterwards require no
   unexplained check failures, the same three `/data` targets, and the exact readiness and staged-edge checks
   above returning 200 without redirects. Back on the client, clone into a different empty directory:

   ```sh
   restored_dir=$(mktemp -d)
   git -c http.followRedirects=false clone --branch main 'https://<external-hostname>/<rehearsal-namespace>/<rehearsal-project>.git' "$restored_dir"
   test "$(git -C "$restored_dir" rev-parse HEAD)" = "$rehearsal_head"
   test "$(cat "$restored_dir/README.md")" = 'rehearsal-only'
   ```

   Require the backed-up commit and original file contents, not the second commit, and verify them in the UI.
   Only then permanently delete this throwaway project through the administrator UI; check both active and
   pending-deletion project lists are empty before proceeding to step 2. Retain the rehearsal archive privately
   as evidence, clearly separate from the real migration archive. **Not checked on a live node:** this entire
   rehearsal, including Gitaly writes and backup/restore through the symlinks. Any failure stops migration before
   old data moves; do not waive it because readiness alone passes.

2. **Check both versions and storage, and retain an untouched recovery copy.**

   On the old node, verify the old package; on the new node repeat the first command in step 1. Both must say
   `19.4.8-ce.0`, Community Edition, not EE. Record backup size estimates and repository storage names privately.

   ```sh
   sudo incus exec --project '<old-project>' '<old-container>' -- dpkg-query -W gitlab-ce
   sudo incus exec --project '<old-project>' '<old-container>' -- du -sh /var/opt/gitlab
   sudo incus exec --project '<old-project>' '<old-container>' -- df -h /var/opt/gitlab/backups
   ```

   On the new node:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- df -h /data /
   sudo incus storage volume show --project '<new-project>' '<pool>' '<data-volume>'
   ```

   Check capacity for the archive, restored data, logs and working space, not just the package. GitLab's restore
   guide requires free backup-directory space of at least three archive sizes; allow more for actual expansion.
   The template's 100GiB data and 40GiB root are starting sizes, not a capacity guarantee. If insufficient, size
   an owner-local template before step 1 and repeat the rehearsal. **Not checked:** this source instance's sizes.

   Schedule downtime and tell users. On the old node, stop GitLab cleanly and then the container; take a stopped
   snapshot before restarting it for the backup:

   ```sh
   sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-ctl stop
   sudo incus stop --project '<old-project>' '<old-container>'
   sudo incus snapshot create --project '<old-project>' '<old-container>' '<pre-migration-snapshot>'
   sudo incus info --project '<old-project>' '<old-container>'
   ```

   Require STOPPED and the snapshot in the listing. Incus instance snapshots do **not** cover attached custom
   volumes or host-path mounts. Inventory devices privately and make a stopped snapshot/copy of each data/config
   mount too before continuing. **Not checked:** those source-specific devices and their backup commands; obtain
   and verify that recovery copy before proceeding. Never modify or delete these recovery objects until acceptance.
   See Incus's [incus snapshot create](https://linuxcontainers.org/incus/docs/main/reference/manpages/incus/snapshot/create/).

3. **Freeze writes, drain jobs and make the final backup in the old container.**

   On the old node, start the original container for the maintenance window:

   ```sh
   sudo incus start --project '<old-project>' '<old-container>'
   sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-ctl status
   ```

   Following *Migrate to a new server*, pause runners, block new jobs, disable Sidekiq periodic jobs in the Admin
   background-jobs page, and wait for CI jobs and Sidekiq busy/enqueued counts to reach zero. CE has no maintenance
   mode: have the operator block user HTTP and SSH ingress while allowing their administrative connection.
   **Not checked:** source-specific ingress and runner controls; record the exact approved commands and successful
   outside denial tests before continuing. Do not assume stopping Puma alone stops SSH writes or scheduled work.

   Once queues are drained, leave only backup dependencies running:

   ```sh
   sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-ctl stop
   sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-ctl start postgresql
   sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-ctl start gitaly
   sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-ctl status
   sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-backup create
   sudo incus exec --project '<old-project>' '<old-container>' -- find /var/opt/gitlab/backups -maxdepth 1 -name '*_gitlab_backup.tar' -printf '%f %s bytes\n'
   ```

   Require a successful backup exit and a new, nonzero archive; record its ID as `<backup-id>` without
   `_gitlab_backup.tar`. The archive does not include `gitlab.rb`, `gitlab-secrets.json`, TLS/SSH keys, Redis or
   queued jobs, external object storage, global server hooks, file hooks and other system files. This procedure drains queued work rather
   than migrating Redis. If any excluded data is used, back it up separately and verify it before continuing.
   For a custom `backup_path`, replace every default backup path in these commands.

4. **Transfer configuration and the backup without exposing secret contents.**

   First stop application processes on the new node:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl stop
   ```

   From the new node, stream `/etc/gitlab` directly over authenticated SSH. This includes `gitlab.rb`,
   `gitlab-secrets.json`, certificates and Docker-image SSH host keys, with no local archive or printed values.
   Use an administrative SSH account with prearranged noninteractive sudo for the source command. Verify the
   SSH host identity first. Do not use `tee`, verbose tar, shell tracing, session recording or Arca exec/logging
   verbs for this transfer. GNU tar's `--no-same-permissions`, with `umask 077`, prevents a permissive source mode
   from making a secret world-readable during extraction:

   ```sh
   ssh '<old-node>' "sudo incus exec --project '<old-project>' '<old-container>' -- tar -C /etc/gitlab -cf - ." |
     sudo incus exec --project '<new-project>' '<new-container>' -- sh -ec 'umask 077; tar --no-same-permissions -C /etc/gitlab -xf -; chmod 600 /etc/gitlab/gitlab.rb /etc/gitlab/gitlab-secrets.json'
   ```

   Check the pipeline succeeds with `pipefail` set, then compare the two checksums for each required file without
   printing its contents. On the new node:

   ```sh
   ssh '<old-node>' "sudo incus exec --project '<old-project>' '<old-container>' -- sha256sum /etc/gitlab/gitlab.rb /etc/gitlab/gitlab-secrets.json"
   sudo incus exec --project '<new-project>' '<new-container>' -- sha256sum /etc/gitlab/gitlab.rb /etc/gitlab/gitlab-secrets.json
   sudo incus exec --project '<new-project>' '<new-container>' -- stat -c '%a %U %n' /etc/gitlab/gitlab.rb /etc/gitlab/gitlab-secrets.json
   ssh '<old-node>' "sudo incus exec --project '<old-project>' '<old-container>' -- tar -C /var/opt/gitlab/backups -cf - '<backup-id>_gitlab_backup.tar'" |
     sudo incus exec --project '<new-project>' '<new-container>' -- sh -ec 'umask 077; tar --no-same-permissions -C /var/opt/gitlab/backups -xf -'
   ssh '<old-node>' "sudo incus exec --project '<old-project>' '<old-container>' -- sha256sum /var/opt/gitlab/backups/'<backup-id>_gitlab_backup.tar'"
   sudo incus exec --project '<new-project>' '<new-container>' -- sha256sum '/var/opt/gitlab/backups/<backup-id>_gitlab_backup.tar'
   sudo incus exec --project '<new-project>' '<new-container>' -- chown git:git '/var/opt/gitlab/backups/<backup-id>_gitlab_backup.tar'
   ```

   Require matching hashes, root ownership and mode 600 for both configuration files. Keep these hashes private
   too. The archive also contains sensitive application data. Configuration is deliberately transferred separately
   from the application backup. If the old SSH keys are under `/etc/ssh` instead of `/etc/gitlab`, transfer only
   its `ssh_host_*` files into the destination `/etc/gitlab` using the same protected stream and compare public-key
   fingerprints; **not checked:** which key location the old installation uses.

5. **Review old configuration locally, prevent background work, then reconfigure.**

   On the new node, review the copied configuration in a private editor without saving it outside the service:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- editor /etc/gitlab/gitlab.rb
   ```

   Keep repository storage names and decryption secrets. Reconcile paths, custom users, external connections,
   SMTP, incoming mail and hooks before starting. Never start a second worker against the old instance's external
   database or Redis. **Not checked:** source-specific settings; stop if they exceed this single-node procedure.
   Ensure GitLab's NGINX remains enabled. At each start the entrypoint writes `/etc/gitlab/arca.rb` and includes it
   last in `gitlab.rb`: the owner's URL, HTTP backend 80, no backend TLS/ACME, and fresh-install root password.
   Restored `gitlab.rb` must not override these template-owned settings or load conflicting settings after
   `arca.rb`. The backend listener uses `gitlab_rails['nginx']['listen_port'] = 80`,
   `gitlab_rails['nginx']['listen_https'] = false` and
   `gitlab_rails['nginx']['redirect_http_to_https'] = false`. In the publisher's
   [19.4.8 NGINX configuration translation](https://gitlab.com/gitlab-org/omnibus-gitlab/-/blob/19.4.8%2Bce.0/files/gitlab-cookbooks/gitlab/libraries/gitlab_rails.rb#L646-658),
   these nested keys take precedence over legacy `nginx[...]` values, so setting only the legacy values would
   leave a restored nested HTTPS listener active. External HTTPS terminates at the approved edge route.

   Temporarily disable background processing and mail on the new service while it is isolated:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- sh -ec 'umask 077; printf "sidekiq[\047enable\047] = false\ngitlab_rails[\047incoming_email_enabled\047] = false\ngitlab_rails[\047smtp_enable\047] = false\n" > /etc/gitlab/arca-migration.rb; printf "\nfrom_file(\047/etc/gitlab/arca-migration.rb\047)\n" >> /etc/gitlab/gitlab.rb'
   arca call action_propose --config '<config>' --args '{"action":"instance_restart","args":{"project":"<new-project>","instance":"<new-container>"}}'
   ```

   Approve the restart. It reruns the entrypoint and `gitlab-ctl reconfigure` after installing the old secrets.
   Wait for that reconfigure to finish before the next commands. Inspect status: Sidekiq must be disabled, the
   database and Gitaly up. Do not expose a route yet. Then stop Puma and confirm it is down:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl stop puma
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl status
   ```

   Require Puma down and no running Sidekiq service; Sidekiq was disabled before this restart, so an additional
   `gitlab-ctl stop sidekiq` can report that its service directory is absent.

6. **Restore, reconfigure and restart the destination.**

   Recheck package version and free disk (step 2). The fresh destination must have no user-created repositories.
   GitLab moves restore targets aside; check they are not nonempty mount roots. Here only `/data` is a mount and
   the restore directories are below it. Run on the new node, answer the restore confirmations, and require exit 0:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-backup restore 'BACKUP=<backup-id>'
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-psql -d gitlabhq_production -c 'SET statement_timeout = 0; ANALYZE VERBOSE;'
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl reconfigure
   arca call action_propose --config '<config>' --args '{"action":"instance_restart","args":{"project":"<new-project>","instance":"<new-container>"}}'
   ```

   Require no skipped/erroring database analysis and successful reconfigure. Approve the restart; the migration
   override still prevents Sidekiq and mail. Check `gitlab-ctl status` and readiness as in step 1. A failed restore
   or readiness check stops the procedure; keep the original and its recovery copy unchanged.

7. **Verify application data before staging the web route.**

   Run GitLab's post-restore checks on the new node and require no unexplained failures:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:check SANITIZE=true
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:doctor:secrets
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:artifacts:check
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:lfs:check
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:uploads:check
   ```

   A disabled Sidekiq warning is expected until step 8; rerun the full check afterwards. Secret checks must decrypt
   restored encrypted values. Keep all diagnostic output private. The sign-in and repository read follow the
   staged route in step 8, before the shared DNS changes.

8. **Move the web address and complete the outside checks.**

   Stop the old container after the backup/transfer (on the old node), and verify STOPPED. Keep it stopped until
   acceptance; do not let old and new process the same incoming mail or background work:

   ```sh
   sudo incus stop --project '<old-project>' '<old-container>'
   sudo incus info --project '<old-project>' '<old-container>'
   ```

   On the new node, with an existing, healthy edge and its certificate credentials configured, propose an edge
   link and route, approving each before the next. `private` reach limits clients to Arca's local/tailnet ranges;
   choose `public` only if the owner intends internet access. The route's hostname must match the supplied URL:

   ```sh
   arca call action_propose --config '<config>' --args '{"action":"service_connect","args":{"from":"<edge-service>","to":"<new-service>"}}'
   arca call action_propose --config '<config>' --args '{"action":"route_add","args":{"hostname":"<external-hostname>","project":"<new-project>","instance":"<new-container>","port":80,"reach":"private"}}'
   arca call route_list --config '<config>'
   ```

   Require the link's port 80, the intended edge destination and `applied: true` on the route result. From outside
   the node, before changing DNS:

   ```sh
   edge_status=$(curl --silent --show-error --max-time 10 --max-redirs 0 --output /dev/null \
     --resolve '<external-hostname>:443:<edge-address>' --write-out '%{http_code}' \
     'https://<external-hostname>/users/sign_in') && test "$edge_status" = 200
   ```

   This request exercises the new edge's own HTTPS-to-HTTP route, keeping the external hostname and certificate
   check while selecting its address before DNS changes. Require 200 with no redirect; a 301 back to the same
   HTTPS URL indicates a backend redirect loop and stops cutover even if container-local readiness passes.

   On an authorized test client, point only that client's name resolution at the new edge for the existing origin.
   **Not checked:** the client's resolver change; verify it before sign-in. Sign in with an existing account and
   2FA, browse representative issues, merge requests, uploads and a known repository file, and compare its
   content/commit with the old instance. Use prompted credentials or a credential manager, never a token in a URL
   or shell history. Require these checks before moving shared DNS.

   The edge terminates HTTPS 443 and sends HTTP to GitLab 80. This template adds no host listener: declared 80
   and 443 alone expose nothing outside the service network, and container 443 has no TLS listener. Neither
   `lan` nor `privatePort` can map several ports; `lan` also binds every host interface. The edge cannot forward
   plain TCP SSH. `publishable: false` leaves web exposure to the explicit route.

   Change the existing DNS A/AAAA records or the existing front proxy to the new edge; remove any stale AAAA path.
   **Not checked:** DNS/front-proxy provider commands and TTL propagation. Record the exact owner-approved change,
   its previous values, and rollback command privately. Require outside name resolution and certificate checks:

   ```sh
   dig +short '<external-hostname>' A
   dig +short '<external-hostname>' AAAA
   edge_status=$(curl --silent --show-error --max-time 10 --max-redirs 0 --output /dev/null \
     --write-out '%{http_code}' 'https://<external-hostname>/users/sign_in') && test "$edge_status" = 200
   git ls-remote 'https://<external-hostname>/<group>/<repository>.git' HEAD
   git remote set-url origin 'https://<external-hostname>/<group>/<repository>.git'
   ```

   Require the intended address, trusted certificate, HTTP 200 without redirects, successful existing-account
   sign-in and the expected HEAD.
   Once the old instance is stopped and the destination is selected, remove the temporary mail/worker override
   on the new node and reconfigure; this restores the copied installation's own settings:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- sed -i '\|^from_file('\''/etc/gitlab/arca-migration.rb'\'')$|d' /etc/gitlab/gitlab.rb
   sudo incus exec --project '<new-project>' '<new-container>' -- rm /etc/gitlab/arca-migration.rb
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl reconfigure
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-ctl restart
   sudo incus exec --project '<new-project>' '<new-container>' -- gitlab-rake gitlab:check SANITIZE=true
   readiness_status=$(sudo incus exec --project '<new-project>' '<new-container>' -- \
     curl --silent --show-error --max-time 10 --max-redirs 0 --output /dev/null \
     --write-out '%{http_code}' 'http://localhost/-/readiness?all=1') && test "$readiness_status" = 200
   ```

   Re-enable periodic jobs and runners only on the new service. Check a disposable project can clone and push
   over HTTPS; check a representative pipeline executes on each runner class and its artifacts upload/download.
   If mail is used, send a test notification and test incoming mail once, checking delivery and absence of duplicate
   processing. **Not checked:** runner credentials, outbound SMTP/incoming-mail network access and these functional
   tests. Arca's private egress restrictions may need owner-approved destinations for internal mail servers.
   **Git over SSH is not served by the shipped template.** Move existing SSH remotes to HTTPS as above and verify
   user access before accepting the cutover. Step 9 is an optional owner-local change, not part of the supported
   web/HTTPS path or the shipped template.

9. **Optional owner-local SSH variant on 2222.**

   The shipped template intentionally has no `privatePort`: `private_port_for` refuses a create when
   `kernel.serviceListenAddresses` is empty, rather than creating an unready listener. A create-time privatePort
   argument cannot add a port the template does not declare. Select this owner-local variant **before step 1**
   when SSH is required. On the new node, edit the installed template/script only, never the shipped repository:

   ```sh
   python3 - '<state-dir>/templates/gitlab.json' '<state-dir>/templates/gitlab.sh' <<'PY'
   import json, pathlib, sys
   template, script = map(pathlib.Path, sys.argv[1:])
   value = json.loads(template.read_text())
   value['ports'] = [80, 443, 2222]
   value['privatePort'] = {'port': 2222}
   template.write_text(json.dumps(value, indent=2) + '\n')
   text = script.read_text()
   text = text.replace("letsencrypt['enable'] = false",
                       "letsencrypt['enable'] = false\ngitlab_rails['gitlab_shell_ssh_port'] = 2222")
   text = text.replace('exec /assets/init-container',
                       "sed -i 's/^Port .*/Port 2222/' /assets/sshd_config\nexec /assets/init-container")
   script.write_text(text)
   PY
   sudo ss -ltn 'sport = :2222'
   ```

   Require no existing listener on the intended address/2222. The node administrator must separately configure
   `kernel.serviceListenAddresses: ["<listen-address>"]` and restart Arca with their approved service-management
   command. **Not checked:** this node's address ownership, service-manager command and firewall/routing. The
   address must exist on a host interface. Reuse step 1's proposal with this addition inside its `args`:

   ```json
   {"privatePort":{"address":"<listen-address>"}}
   ```

   Its card must name port 2222 on that address only. The listener and GitLab's OpenSSH daemon **both** use
   2222; setting only `gitlab_shell_ssh_port` changes advertised clone URLs, not OpenSSH's actual listener. The
   script changes `/assets/sshd_config` on each start, including after a rebuild. Verify after the empty start:

   ```sh
   sudo incus exec --project '<new-project>' '<new-container>' -- /usr/sbin/sshd -t -f /assets/sshd_config
   sudo incus exec --project '<new-project>' '<new-container>' -- sh -ec '/usr/sbin/sshd -T -f /assets/sshd_config | grep "^port "'
   arca call service_status --config '<config>' --args '{"name":"<new-service>"}'
   sudo ss -ltn 'sport = :2222'
   ```

   Require `port 2222`, `privatePort.ready: true`, and only the intended host address bound. The GitLab 19.4.8
   image's `/assets/sshd_config` has `Port 22`; its later 19.6 drop-in mechanism must not be assumed here. The
   host proxy bypasses the service NIC ACL; host firewall policy and reachability control who can reach it.
   The port number is the same outside and inside, so binding 22 could collide with the node's administrative SSH.
   Port 22 is not declared by either the shipped template or this variant.

   At cutover, use the preserved GitLab SSH host keys and verify their fingerprints through a trusted channel.
   From an outside client whose SSH key is already registered in GitLab:

   ```sh
   ssh -T -p 2222 'git@<external-hostname>'
   git ls-remote 'ssh://git@<external-hostname>:2222/<group>/<repository>.git' HEAD
   git remote set-url origin 'ssh://git@<external-hostname>:2222/<group>/<repository>.git'
   ```

   Require the expected GitLab welcome (its no-shell session can exit nonzero), expected HEAD and a successful
   clone/push on a disposable project. Existing `git@<external-hostname>:<group>/<repository>.git` remotes assume
   22: each user must change its URL as above or configure SSH `Host <external-hostname>` with `Port 2222`.
   If preserving port 22 is mandatory, an independently approved external TCP forwarder must map 22 to this
   listener; Arca's edge cannot do that. **Not checked:** such forwarding and user remote changes. Do not silently
   redirect existing Git remotes to the node's administrative SSH server. With no SSH variant, explicitly migrate
   remotes to HTTPS and record user acceptance before cutover.

10. **Accept or roll back, preserving the old instance for at least seven days.**

    Record acceptance only after web/sign-in, HTTPS repository read/write, existing SSH remotes moved to HTTPS,
    runners, mail where used, secret decryption and data checks pass. Keep the old container stopped, its snapshot,
    every attached-volume recovery copy, and the backup/configuration material for **at least seven days after
    written acceptance**, longer if the owner's retention policy requires it. Seven days is this runbook's proposed
    retention, not a GitLab guarantee. Never remove the old instance during this procedure.

    If acceptance fails before new writes are allowed, stop the new service on the new node through an approved
    proposal, then verify STOPPED with `service_status`:

    ```sh
    arca call action_propose --config '<config>' --args '{"action":"instance_stop","args":{"project":"<new-project>","instance":"<new-container>"}}'
    arca call service_status --config '<config>' --args '{"name":"<new-service>"}'
    ```

    Revert the DNS/front-proxy and any SSH forwarding using the privately recorded inverse from step 8/9. On the
    old node, with its original configuration and volumes still intact:

    ```sh
    sudo incus start --project '<old-project>' '<old-container>'
    sudo incus exec --project '<old-project>' '<old-container>' -- gitlab-ctl start
    readiness_status=$(sudo incus exec --project '<old-project>' '<old-container>' -- \
      curl --silent --show-error --max-time 10 --max-redirs 0 --output /dev/null \
      --write-out '%{http_code}' 'http://localhost/-/readiness?all=1') && test "$readiness_status" = 200
    ```

    Restore the old ingress rules and runner/periodic-job controls from step 3; repeat the outside URL, sign-in,
    repository and mail checks against the old destination. **Not checked:** those inverse infrastructure commands.
    If the original was damaged, use the retained recovery objects, including its data volumes, under a separately
    checked recovery procedure; do not overwrite the only recovery copy. If users or jobs have written to the new
    service, freeze both sides first: simple DNS rollback loses those writes. Reconcile or back up/restore that
    new data deliberately before choosing one writer. Never let both installations accept writes concurrently.
