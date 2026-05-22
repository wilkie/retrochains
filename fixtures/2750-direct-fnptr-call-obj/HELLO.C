int (*global_fn)(int);
int main(void) {
  return global_fn(42);
}
