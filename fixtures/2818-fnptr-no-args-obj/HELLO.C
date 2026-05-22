int (*hook)(void);
int trigger(void) {
  return hook();
}
