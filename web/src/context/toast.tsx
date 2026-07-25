import { createContext, createSignal, For, ParentProps, useContext } from 'solid-js'
import { CheckCircle2, CircleAlert, Info, X } from 'lucide-solid'

type ToastKind = 'success' | 'error' | 'info'
interface Toast { id: number; message: string; kind: ToastKind }

interface ToastContextValue {
  notify: (message: string, kind?: ToastKind) => void
}

const ToastContext = createContext<ToastContextValue>()

export function ToastProvider(props: ParentProps) {
  const [toasts, setToasts] = createSignal<Toast[]>([])
  let nextId = 0

  const remove = (id: number) => setToasts((items) => items.filter((item) => item.id !== id))
  const notify = (message: string, kind: ToastKind = 'info') => {
    const id = ++nextId
    setToasts((items) => [...items, { id, message, kind }])
    window.setTimeout(() => remove(id), 4200)
  }

  return (
    <ToastContext.Provider value={{ notify }}>
      {props.children}
      <div class="toast-stack" aria-live="polite">
        <For each={toasts()}>
          {(toast) => (
            <div class={`toast toast-${toast.kind}`}>
              {toast.kind === 'success' ? <CheckCircle2 size={18} /> : toast.kind === 'error' ? <CircleAlert size={18} /> : <Info size={18} />}
              <span>{toast.message}</span>
              <button class="icon-button" onClick={() => remove(toast.id)} aria-label="关闭通知"><X size={16} /></button>
            </div>
          )}
        </For>
      </div>
    </ToastContext.Provider>
  )
}

export function useToast() {
  const context = useContext(ToastContext)
  if (!context) throw new Error('ToastProvider is missing')
  return context
}
