import type { PropsWithChildren } from "react";
import { Navigate } from "react-router-dom";
import { useAuth } from "@/auth/AuthContext";
import { shouldShowLocalSetup } from "@/auth/setup";
import { useEffect, useState } from "react";
import { BootScreen } from "@/components/BootScreen";

export default function ProtectedRoute({ children }: PropsWithChildren) {
  const { isAuthenticated, isTauri, isInitializing } = useAuth();
  const [needsLocalSetup, setNeedsLocalSetup] = useState<boolean | null>(null);

  // In Tauri mode, check whether the local sidecar setup is still needed.
  // A selected remote instance should go to login even if local first-run is
  // incomplete.
  useEffect(() => {
    if (!isTauri || isInitializing) return;
    if (isAuthenticated) {
      setNeedsLocalSetup(false);
      return;
    }
    let cancelled = false;
    (async () => {
      const needsSetup = await shouldShowLocalSetup();
      if (!cancelled) setNeedsLocalSetup(needsSetup);
    })();
    return () => {
      cancelled = true;
    };
  }, [isTauri, isInitializing, isAuthenticated]);

  // In Tauri mode, show a spinner while initializing or while the
  // setup check is still pending (needsLocalSetup === null).
  if (
    isTauri &&
    (isInitializing || (!isAuthenticated && needsLocalSetup === null))
  ) {
    return <BootScreen />;
  }

  // In Tauri mode, only force setup for the local sidecar.
  if (isTauri && !isAuthenticated && needsLocalSetup === true) {
    return <Navigate to="/setup" replace />;
  }

  if (!isAuthenticated) {
    return <Navigate to="/login" replace />;
  }
  return <>{children}</>;
}
