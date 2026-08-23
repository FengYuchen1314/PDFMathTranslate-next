import AdminPage from "./AdminPage";
import TranslationPage from "./TranslationPage";

export default function App() {
  const isAdmin =
    window.location.pathname === "/admin" ||
    window.location.pathname.startsWith("/admin/");

  return isAdmin ? <AdminPage /> : <TranslationPage />;
}
