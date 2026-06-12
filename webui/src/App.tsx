import { Routes, Route, Navigate } from "react-router-dom";
import Login from "@/auth/Login";
import ProtectedRoute from "@/auth/ProtectedRoute";
import Layout from "@/components/Layout";
import Dashboard from "@/pages/Dashboard";
import Providers from "@/pages/Providers";
import RoutesPage from "@/pages/Routes";
import ApiKeys from "@/pages/ApiKeys";
import OAuth from "@/pages/OAuth";
import RequestLogs from "@/pages/RequestLogs";
import Audit from "@/pages/Audit";
import IntegrationGuide from "@/pages/IntegrationGuide";

export default function App() {
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      <Route
        element={
          <ProtectedRoute>
            <Layout />
          </ProtectedRoute>
        }
      >
        <Route index element={<Dashboard />} />
        <Route path="providers" element={<Providers />} />
        <Route path="routes" element={<RoutesPage />} />
        <Route path="api-keys" element={<ApiKeys />} />
        <Route path="oauth" element={<OAuth />} />
        <Route path="requests" element={<RequestLogs />} />
        <Route path="audit" element={<Audit />} />
        <Route path="integration" element={<IntegrationGuide />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
