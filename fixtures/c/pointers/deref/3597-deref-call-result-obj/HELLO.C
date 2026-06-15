int *get(void);

int via(void) {
  return *get();
}
