# GitLab CE's own init stays PID 1 after preparing Arca's one persistent volume.
set -eu
umask 077

# Parse data as a URI, never as Ruby code, and never print the owner's value.
/opt/gitlab/embedded/bin/ruby -ruri -e '
  begin
    url = URI.parse(ENV.fetch("GITLAB_EXTERNAL_URL", ""))
    valid = %w[http https].include?(url.scheme) && url.host && !url.host.empty?
  rescue URI::InvalidURIError
    valid = false
  end
  unless valid
    warn "Set GITLAB_EXTERNAL_URL to an http or https address before starting GitLab."
    exit 1
  end
'

# Seed each directory only once. A rebuild gives a new root and the same /data;
# a restart finds the links already there. Never copy image defaults over kept data.
chmod 0755 /data
for pair in config:/etc/gitlab data:/var/opt/gitlab logs:/var/log/gitlab; do
    name=${pair%%:*}
    path=${pair#*:}
    if [ ! -d "/data/$name" ]; then
        cp -a "$path" "/data/$name"
    fi
    rm -rf "$path"
    ln -s "/data/$name" "$path"
done

# Load last, including after an old gitlab.rb is restored. Only these settings
# belong to this template; repository storage names and other old settings stay.
cat > /etc/gitlab/arca.rb <<'RUBY'
external_url ENV.fetch('GITLAB_EXTERNAL_URL')
# In 19.4.8, nested keys win over legacy nginx settings in a restored file.
gitlab_rails['nginx']['listen_port'] = 80
gitlab_rails['nginx']['listen_https'] = false
gitlab_rails['nginx']['redirect_http_to_https'] = false
letsencrypt['enable'] = false
gitlab_rails['initial_root_password'] = ENV.fetch('GITLAB_ROOT_PASSWORD')
RUBY
touch /etc/gitlab/gitlab.rb
include="from_file('/etc/gitlab/arca.rb')"
if ! grep -Fxq "$include" /etc/gitlab/gitlab.rb; then
    printf '\n%s\n' "$include" >> /etc/gitlab/gitlab.rb
fi
chmod 0600 /etc/gitlab/gitlab.rb /etc/gitlab/arca.rb

# The publisher's init uses find without -H. Follow our data link too so a
# restart after an unclean stop removes stale sockets/PIDs on the kept volume.
find -H /var/opt/gitlab -maxdepth 4 \( -type f -o -type s \) \
    \( -name pid -o -name '*.pid' -o -name 'socket.?' \) -delete
exec /assets/init-container
