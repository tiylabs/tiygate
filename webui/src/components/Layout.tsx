import { useEffect, useState } from "react";
import { NavLink, Outlet } from "react-router-dom";
import { useTranslation } from "react-i18next";
import * as RDialog from "@radix-ui/react-dialog";
import {
  LayoutDashboard,
  Server,
  Route as RouteIcon,
  KeyRound,
  ShieldCheck,
  ScrollText,
  ListChecks,
  LogOut,
  Menu,
  X,
  Waypoints,
  PanelLeftClose,
  PanelLeftOpen,
  type LucideIcon,
} from "lucide-react";
import { useAuth } from "@/auth/AuthContext";
import { LanguageSwitcher } from "./LanguageSwitcher";
import { ThemeSwitcher } from "./ThemeSwitcher";
import { cn } from "@/lib/cn";

const navItems: Array<{
  to: string;
  end?: boolean;
  key: string;
  icon: LucideIcon;
}> = [
  { to: "/", end: true, key: "nav.dashboard", icon: LayoutDashboard },
  { to: "/providers", key: "nav.providers", icon: Server },
  { to: "/routes", key: "nav.routes", icon: RouteIcon },
  { to: "/api-keys", key: "nav.apiKeys", icon: KeyRound },
  { to: "/oauth", key: "nav.oauth", icon: ShieldCheck },
  { to: "/requests", key: "nav.requests", icon: ScrollText },
  { to: "/audit", key: "nav.audit", icon: ListChecks },
];

function navLinkClass({ isActive }: { isActive: boolean }): string {
  return cn(
    "group relative flex items-center gap-2.5 rounded-md px-3 py-2 text-sm font-medium",
    "transition-colors duration-[var(--duration-fast)]",
    "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
    "before:absolute before:left-0 before:top-1.5 before:bottom-1.5 before:w-0.5 before:rounded-full",
    "before:transition-opacity before:duration-[var(--duration-fast)] before:bg-primary",
    isActive
      ? "bg-primary-soft text-primary before:opacity-100"
      : "text-text-muted before:opacity-0 hover:bg-surface-muted hover:text-text",
  );
}

function SidebarContent({
  onNavigate,
  collapsed = false,
  onToggleCollapse,
}: {
  onNavigate?: () => void;
  collapsed?: boolean;
  onToggleCollapse?: () => void;
}) {
  const { t } = useTranslation();
  const { logout } = useAuth();

  return (
    <div className="flex h-full flex-col">
      <div
        className={cn(
          "flex h-14 shrink-0 items-center border-b border-border",
          collapsed ? "justify-center px-2" : "gap-2.5 px-4",
        )}
      >
        {collapsed ? (
          onToggleCollapse && (
            <button
              type="button"
              aria-label={t("app.expandSidebar")}
              title={t("app.expandSidebar")}
              onClick={onToggleCollapse}
              className="group/logo inline-flex h-8 w-8 items-center justify-center rounded-md text-primary transition-colors hover:bg-surface-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              <Waypoints
                size={16}
                aria-hidden
                className="group-hover/logo:hidden"
              />
              <PanelLeftOpen
                size={16}
                aria-hidden
                className="hidden text-text-muted group-hover/logo:block"
              />
            </button>
          )
        ) : (
          <>
            <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-primary-soft text-primary">
              <Waypoints size={16} aria-hidden />
            </span>
            <span className="truncate text-sm font-semibold tracking-[-0.01em] text-text">
              {t("app.title")}
            </span>
            {onToggleCollapse && (
              <button
                type="button"
                aria-label={t("app.collapseSidebar")}
                title={t("app.collapseSidebar")}
                onClick={onToggleCollapse}
                className="ml-auto inline-flex h-8 w-8 items-center justify-center rounded-md text-text-muted transition-colors hover:bg-surface-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <PanelLeftClose size={16} />
              </button>
            )}
          </>
        )}
      </div>
      <nav className="flex-1 space-y-0.5 overflow-y-auto p-2">
        {navItems.map((item) => {
          const Icon = item.icon;
          const label = t(item.key);
          return (
            <NavLink
              key={item.to}
              to={item.to}
              end={item.end}
              onClick={onNavigate}
              title={collapsed ? label : undefined}
              aria-label={collapsed ? label : undefined}
              className={({ isActive }) =>
                cn(navLinkClass({ isActive }), collapsed && "justify-center px-0")
              }
            >
              <Icon
                size={16}
                aria-hidden
                className="shrink-0 opacity-80 transition-opacity duration-[var(--duration-fast)] group-hover:opacity-100"
              />
              {!collapsed && <span className="truncate">{label}</span>}
            </NavLink>
          );
        })}
      </nav>
      <div className={cn("space-y-2 border-t border-border", collapsed ? "p-2" : "p-3")}>
        {collapsed ? (
          <div className="flex flex-col items-center gap-2">
            <ThemeSwitcher />
            <button
              type="button"
              aria-label={t("app.logout")}
              title={t("app.logout")}
              className="inline-flex h-8 w-8 items-center justify-center rounded-md text-danger transition-colors hover:bg-danger-soft focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              onClick={logout}
            >
              <LogOut size={16} aria-hidden />
            </button>
          </div>
        ) : (
          <>
            <div className="flex items-center justify-between">
              <LanguageSwitcher />
              <ThemeSwitcher />
            </div>
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm text-danger transition-colors hover:bg-danger-soft focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              onClick={logout}
            >
              <LogOut size={16} aria-hidden />
              {t("app.logout")}
            </button>
          </>
        )}
      </div>
    </div>
  );
}

export default function Layout() {
  const { t } = useTranslation();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [collapsed, setCollapsed] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.localStorage.getItem("sidebar-collapsed") === "1";
  });

  useEffect(() => {
    window.localStorage.setItem("sidebar-collapsed", collapsed ? "1" : "0");
  }, [collapsed]);

  return (
    <div className="flex min-h-full bg-bg">
      {/* Desktop sidebar */}
      <aside
        className={cn(
          "relative hidden shrink-0 border-r border-border bg-surface transition-[width] duration-[var(--duration-fast)] lg:block",
          collapsed ? "w-16" : "w-56",
        )}
      >
        <SidebarContent
          collapsed={collapsed}
          onToggleCollapse={() => setCollapsed((value) => !value)}
        />
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        {/* Mobile top bar */}
        <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border bg-surface px-4 lg:hidden">
          <RDialog.Root open={drawerOpen} onOpenChange={setDrawerOpen}>
            <RDialog.Trigger asChild>
              <button
                type="button"
                aria-label={t("app.menu")}
                className="inline-flex h-9 w-9 items-center justify-center rounded-md text-text-muted transition-colors hover:bg-surface-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <Menu size={18} />
              </button>
            </RDialog.Trigger>
            <RDialog.Portal>
              <RDialog.Overlay className="animate-overlay-in fixed inset-0 z-40 bg-overlay lg:hidden" />
              <RDialog.Content className="animate-drawer-in fixed inset-y-0 left-0 z-50 w-64 border-r border-border bg-surface focus:outline-none lg:hidden">
                <RDialog.Title className="sr-only">{t("app.menu")}</RDialog.Title>
                <RDialog.Close asChild>
                  <button
                    type="button"
                    aria-label={t("app.closeMenu")}
                    className="absolute right-2 top-3 z-10 inline-flex h-8 w-8 items-center justify-center rounded-md text-text-subtle hover:bg-surface-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    <X size={18} />
                  </button>
                </RDialog.Close>
                <SidebarContent onNavigate={() => setDrawerOpen(false)} />
              </RDialog.Content>
            </RDialog.Portal>
          </RDialog.Root>
          <span className="text-sm font-semibold text-text">
            {t("app.title")}
          </span>
        </header>

        <main className="min-w-0 flex-1 overflow-y-auto p-4 sm:p-6">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
