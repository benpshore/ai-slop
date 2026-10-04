import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "TPE · Private extraction alpha",
  description: "Extract documents, web pages and feeds with retained sources, links and provenance.",
  icons: {
    icon: "/favicon.svg",
    shortcut: "/favicon.svg",
  },
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body className="antialiased">{children}</body>
    </html>
  );
}
