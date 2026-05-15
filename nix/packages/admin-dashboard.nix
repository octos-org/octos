{
  buildNpmPackage,
  importNpmLock,
}:

let
  src = ../../dashboard;
  packageJson = builtins.fromJSON (builtins.readFile (src + "/package.json"));
in

buildNpmPackage {
  inherit src;
  pname = packageJson.name;
  version = packageJson.version;

  npmDeps = importNpmLock { npmRoot = src; };
  npmConfigHook = importNpmLock.npmConfigHook;

  doCheck = false;

  buildPhase = ''
    export VITE_BASE_PATH=/admin/
    export VITE_OUT_DIR=$out/admin
    npm run build
  '';
}
