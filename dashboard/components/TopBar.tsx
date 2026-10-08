"use client";

import Link from "next/link";
import type { ReactNode } from "react";
import { API_BASE } from "@/lib/api";

export function TopBar({ crumbs, actions }: { crumbs?: ReactNode; actions?: ReactNode }) {
  return (
    <header className="topbar">
      <Link href="/" className="brand">
        <span className="logo">◒</span> Soil
      </Link>
      <nav className="crumbs">{crumbs}</nav>
      <div className="topbar-actions">{actions}</div>
      <span className="api-pill" title="Control plane API">
        {API_BASE.replace(/^https?:\/\//, "")}
      </span>
    </header>
  );
}

export function ErrorBanner({ error, onClose }: { error?: string; onClose?: () => void }) {
  if (!error) return null;
  return (
    <div className="banner error">
      <span>{error}</span>
      {onClose ? (
        <button className="icon" onClick={onClose}>
          ×
        </button>
      ) : null}
    </div>
  );
}
