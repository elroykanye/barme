import { createContext, useContext, useState, type ReactNode } from "react";
import { api, loadCreds, saveCreds, type Creds } from "./api";

interface AuthValue {
  creds: Creds | null;
  authed: boolean;
  login: (access: string, secret: string) => Promise<void>;
  logout: () => void;
}

const AuthContext = createContext<AuthValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [creds, setCreds] = useState<Creds | null>(loadCreds());

  async function login(access: string, secret: string) {
    // Validate by hitting an owner-only endpoint with the new credentials.
    saveCreds({ access, secret });
    try {
      await api.listBuckets();
      setCreds({ access, secret });
    } catch (e) {
      saveCreds(creds); // roll back to whatever we had
      throw e;
    }
  }

  function logout() {
    saveCreds(null);
    setCreds(null);
  }

  return (
    <AuthContext.Provider value={{ creds, authed: creds !== null, login, logout }}>
      {children}
    </AuthContext.Provider>
  );
}

export function useAuth() {
  const v = useContext(AuthContext);
  if (!v) throw new Error("useAuth outside AuthProvider");
  return v;
}
