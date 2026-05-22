int arr[10];
int getidx(void);
void store(int v) {
  arr[getidx()] = v;
}
