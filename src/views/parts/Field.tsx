/**
 * 设置项的统一排版：左边说明、右边控件。
 *
 * 说明文字不是可选的——每个参数都要讲清楚「调它会怎样」，
 * 否则高级设置就变成一堆没人敢动的数字。
 */
import type { ReactNode } from "react";

import { Slider } from "@/components/ui/slider";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

export function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-1">
      {/* 不加 uppercase：标题里含 ".mp4" 这类扩展名，大写后变成 ".MP4" 反而失真。 */}
      <h2 className="px-1 pb-1 text-xs font-semibold tracking-wide text-muted-foreground">
        {title}
      </h2>
      <div className="divide-y divide-border overflow-hidden rounded-lg border border-border bg-card">
        {children}
      </div>
    </section>
  );
}

export function Row({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center gap-4 px-3 py-2.5">
      <div className="min-w-0 flex-1">
        <div className="text-[13px]">{label}</div>
        {hint && <div className="text-xs leading-snug text-muted-foreground">{hint}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-3">{children}</div>
    </div>
  );
}

/** 数值滑块 + 右侧读数。读数用等宽字体，拖动时不会左右抖。 */
export function NumberRow({
  label,
  hint,
  value,
  min,
  max,
  step = 1,
  unit,
  format,
  onChange,
}: {
  label: string;
  hint?: ReactNode;
  value: number;
  min: number;
  max: number;
  step?: number;
  unit?: string;
  format?: (v: number) => string;
  onChange: (v: number) => void;
}) {
  return (
    <Row label={label} hint={hint}>
      <Slider
        className="w-40"
        value={[value]}
        min={min}
        max={max}
        step={step}
        onValueChange={([v]) => onChange(v)}
      />
      <span className="w-20 text-right font-mono text-xs text-muted-foreground">
        {format ? format(value) : `${value}${unit ?? ""}`}
      </span>
    </Row>
  );
}

export function SwitchRow({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string;
  hint?: ReactNode;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <Row label={label} hint={hint}>
      <Switch checked={checked} onCheckedChange={onChange} />
    </Row>
  );
}

/**
 * 单行文本输入。
 *
 * `invalid` 只染红边框，不弹任何东西——这一行的错误说明写在 `hint` 里，
 * 一处说清楚就够，两处会打架。
 */
export function TextRow({
  label,
  hint,
  value,
  placeholder,
  invalid,
  onChange,
}: {
  label: string;
  hint?: ReactNode;
  value: string;
  placeholder?: string;
  invalid?: boolean;
  onChange: (v: string) => void;
}) {
  return (
    <Row label={label} hint={hint}>
      <input
        className={`w-56 rounded-md border bg-background px-2 py-1 font-mono text-xs outline-none focus-visible:ring-[3px] ${
          invalid
            ? "border-destructive ring-destructive/20"
            : "border-input focus-visible:border-ring focus-visible:ring-ring/50"
        }`}
        value={value}
        placeholder={placeholder}
        spellCheck={false}
        autoComplete="off"
        autoCapitalize="off"
        onChange={(e) => onChange(e.target.value)}
      />
    </Row>
  );
}

export function SelectRow<T extends string>({
  label,
  hint,
  value,
  options,
  onChange,
}: {
  label: string;
  hint?: ReactNode;
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
}) {
  return (
    <Row label={label} hint={hint}>
      <Select value={value} onValueChange={(v) => onChange(v as T)}>
        <SelectTrigger className="w-44">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {options.map((o) => (
            <SelectItem key={o.value} value={o.value}>
              {o.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </Row>
  );
}
