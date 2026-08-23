{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  nativeBuildInputs = [ pkgs.pkg-config pkgs.cargo pkgs.rustc ];
  buildInputs = [ pkgs.openssl ];
}
