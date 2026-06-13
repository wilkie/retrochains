int main(void) {
  char a[5];
  char *p = a + 1;
  char *q = a + 4;
  return q - p;
}
