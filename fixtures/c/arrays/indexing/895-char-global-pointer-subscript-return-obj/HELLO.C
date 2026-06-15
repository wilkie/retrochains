char a[3];
char *p;
int main() {
  p = a;
  a[1] = 7;
  return p[1];
}
