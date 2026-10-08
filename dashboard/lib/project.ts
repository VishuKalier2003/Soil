"use client";

import { useEffect, useState } from "react";

const KEY = "soil.project";

/** The selected control-plane project (persisted per browser). */
export function useProject(): [string, (p: string) => void] {
  const [project, setProject] = useState("default");
  useEffect(() => {
    try {
      const saved = localStorage.getItem(KEY);
      if (saved) setProject(saved);
    } catch {
      // storage unavailable: keep the default
    }
  }, []);
  const set = (p: string) => {
    setProject(p);
    try {
      localStorage.setItem(KEY, p);
    } catch {
      // ignore
    }
  };
  return [project, set];
}
