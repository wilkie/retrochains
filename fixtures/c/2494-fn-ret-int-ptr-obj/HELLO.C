int storage;
int *get_storage(void) {
  return &storage;
}
int main(void) {
  int *p;
  p = get_storage();
  *p = 77;
  return storage;
}
