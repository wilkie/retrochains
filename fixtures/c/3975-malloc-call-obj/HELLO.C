void *malloc(unsigned);
void free(void *);
int main(void) {
  void *p = malloc(100);
  free(p);
  return 0;
}
