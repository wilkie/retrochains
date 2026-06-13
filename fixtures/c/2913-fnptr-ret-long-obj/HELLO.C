long (*op)(void);
long call(void) {
  return op();
}
