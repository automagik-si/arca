# The source build.sh puts beside GitHub's template in place of their azure-arm one: Packer's null builder, which
# runs the provisioners over SSH on a machine that already exists, here the Incus virtual machine build.sh started.

variable "arca_ssh_host" {
  type    = string
  default = ""
}

variable "arca_ssh_key" {
  type    = string
  default = ""
}

source "null" "arca" {
  communicator         = "ssh"
  ssh_host             = var.arca_ssh_host
  ssh_username         = "packer"
  ssh_private_key_file = var.arca_ssh_key
  ssh_timeout          = "20m"
}
