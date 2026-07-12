{ lib, ... }:

{
  imports = [ ./testing.nix ];

  boot.loader.grub.enable = true;
  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_scsi"
    "virtio_blk"
    "virtio_net"
    "ahci"
    "xhci_pci"
    "sd_mod"
    "sr_mod"
    "nvme"
  ];
  boot.kernelParams = [ "console=ttyS0,115200" ];

  networking = {
    hostName = "mageos-testing";
    domain = "bougie.tools";
    useDHCP = lib.mkDefault true;
    interfaces.enp8s0.ipv6.addresses = [
      {
        address = "2a01:4f9:3081:36c8::1";
        prefixLength = 64;
      }
    ];
    defaultGateway6 = {
      address = "fe80::1";
      interface = "enp8s0";
    };
    firewall = {
      enable = true;
      allowedTCPPorts = [ 22 80 443 ];
    };
  };

  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
    };
  };

  users.mutableUsers = false;
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMAlEwhbBOJor7VO1Bkv7jLM4aTzElFGSdduEMIz73d7 jelle@dev-debn-02"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICunYiTe1MOJsGC5OBn69bewMBS5bCCE1WayvM4DZLwE jelle@Jelles-MacBook-Pro.local"
  ];

  system.autoUpgrade = {
    enable = true;
    flake = "github:cresset-tools/infra#mageos-testing";
    # Staggered after the other hosts; the nightly test run starts 01:30
    # and is long done by upgrade time.
    dates = "Sun 06:00";
    randomizedDelaySec = "30min";
    allowReboot = true;
    rebootWindow = {
      lower = "05:30";
      upper = "07:30";
    };
  };

  services.journald.extraConfig = ''
    SystemMaxUse=500M
  '';

  system.stateVersion = "25.11";
}
