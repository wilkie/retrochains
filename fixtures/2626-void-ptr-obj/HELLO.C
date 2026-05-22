int x = 42;
void *vp = &x;
int main(void) {
  int *ip;
  ip = (int *)vp;
  return *ip;
}
