{
  buildNpmPackage,
  importNpmLock,
}:
buildNpmPackage {
  pname = "octos-admin-dashboard";
  version = "0.1.0";
  src = ../../dashboard;

  npmDeps = importNpmLock { npmRoot = ../../dashboard; };
  npmConfigHook = importNpmLock.npmConfigHook;

  doCheck = false;

  buildPhase = ''
    export VITE_BASE_PATH=/admin/
    export VITE_OUT_DIR=$out/admin
    npm run build
  '';
}
