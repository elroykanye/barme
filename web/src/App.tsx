import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { AuthProvider, useAuth } from "@/lib/auth";
import { ToastProvider } from "@/lib/toast";
import { DialogProvider } from "@/lib/dialogs";
import { Layout } from "@/components/Layout";
import { Login } from "@/routes/Login";
import { Home } from "@/routes/Home";
import { BucketView } from "@/routes/Bucket";
import { PotSettings } from "@/routes/PotSettings";
import { Settings } from "@/routes/Settings";
import { SearchPage } from "@/routes/Search";
import { Status } from "@/routes/Status";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
});

function Gate() {
  const { authed } = useAuth();
  if (!authed) return <Login />;
  return (
    <Routes>
      <Route element={<Layout />}>
        <Route path="/" element={<Home />} />
        <Route path="/search" element={<SearchPage />} />
        <Route path="/status" element={<Status />} />
        <Route path="/settings" element={<Settings />} />
        <Route path="/p/:bucket" element={<BucketView />} />
        <Route path="/p/:bucket/settings" element={<PotSettings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <ToastProvider>
        <DialogProvider>
          <AuthProvider>
            <BrowserRouter>
              <Gate />
            </BrowserRouter>
          </AuthProvider>
        </DialogProvider>
      </ToastProvider>
    </QueryClientProvider>
  );
}
