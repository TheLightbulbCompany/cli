// Derived from Dyad's Next.js template (https://github.com/dyad-sh/nextjs-template), commit 121d5674, src/app/layout.tsx. Copyright 2025 Dyad Tech, Inc. Apache-2.0. Modified by The Lightbulb Company.
import type { Metadata } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import { ErrorReporter } from "@/app/_lightbulb/error-reporter";
import "@/app/globals.css";

const geistSans = Geist({
  variable: "--font-geist-sans",
  subsets: ["latin"],
});

const geistMono = Geist_Mono({
  variable: "--font-geist-mono",
  subsets: ["latin"],
});

export const metadata: Metadata = {
  title: "demo-content",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body className={`${geistSans.variable} ${geistMono.variable} font-sans antialiased`}>
        <ErrorReporter />
        {children}
      </body>
    </html>
  );
}
