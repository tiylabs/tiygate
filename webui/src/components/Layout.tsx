import { NavLink, Outlet } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  LayoutDashboard,
  Server,
  Route as RouteIcon,
  KeyRound,
  ShieldCheck,
  ScrollText,
  ListChecks,
  LogOut,
} from "lucide-react";
import { useAuth } from "@/auth/AuthContext";
import { LanguageSwitcher } from "./LanguageSwitcher";
import { cn } from "@/lib/cn";

const navItems = [
  { to: "/", end: true, key: "nav.dashboard", icon: LayoutDashboard },
  { to: "/providers", key: "nav.providers", icon: Server },
  { to: "/routes", key: "nav.routes", icon: RouteIcon },
  { to: "/api-keys", key: "nav.apiKeys", icon: KeyRound },
  { to: "/oauth", key: "nav.oauth", icon: ShieldCheck },
  { to: "/requests", key: "nav.requests", icon: ScrollText },
  { to: "/audit", key: "nav.audit", icon: ListChecks },
];

export default function Layout() {
  const { t } = useTranslation();
  const { logout } = useAuth();

  return (
    <div className="flex min-h-full bg-slate-50">
      <aside className="flex w-56 flex-col border-r border-slate-200 bg-white">
        <div className="flex h-14 items-center border-b border-slate-100 px-4">
          <span className="text-sm font-semibold text-slate-800">
            {t("app.title")}
          </span>
        </div>
        <nav className="flex-1 space-y-1 p-2">
          {navItems.map((item) => {
            const Icon = item.icon;
            return (
              <NavLink
                key={item.to}
                to={item.to}
                end={item.end}
                className={({ isActive }) =>
                  cn(
                    "flex items-center gap-2 rounded-md px-3 py-2 text-sm font-medium",
                    isActive
                      ? "bg-slate-900 text-white"
                      : "text-slate-600 hover:bg-slate-100",
                  )
                }
              >
                <Icon size={16} />
                {t(item.key)}
              </NavLink>
            );
          })}
        </nav>
        <div className="space-y-2 border-t border-slate-100 p-3">
          <LanguageSwitcher />
          <button
            className="flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm text-slate-600 hover:bg-slate-100"
            onClick={logout}
          >
            <LogOut size={16} />
            {t("app.logout")}
          </button>
        </div>
      </aside>
      <main className="flex-1 overflow-y-auto p-6">
        <Outlet />
      </main>
    </div>
  );
}
