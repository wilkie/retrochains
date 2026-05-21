void alloc_int(int **pp) {
  static int storage = 42;
  *pp = &storage;
}
int main(void) {
  int *p;
  alloc_int(&p);
  return *p;
}
