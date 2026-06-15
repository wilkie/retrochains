long a[3];
int main(void) {
  long *p;
  long v;
  p = a;
  v = *p++;
  return (int)v;
}
